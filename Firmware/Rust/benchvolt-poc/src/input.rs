use core::cell::RefCell;

use cortex_m::interrupt::Mutex;
use heapless::Deque;
use stm32f0xx_hal::pac::{self, interrupt};

const ENCODER_ACCELERATION_IDLE_MS: u16 = 80;

#[derive(Clone, Copy)]
struct EncoderEvent {
    direction: i8,
    tick: u16,
}

static ENCODER_EVENTS: Mutex<RefCell<Deque<EncoderEvent, 16>>> =
    Mutex::new(RefCell::new(Deque::new()));
static ENCODER_EDGE_COUNT: Mutex<RefCell<u32>> = Mutex::new(RefCell::new(0));
static ENCODER_DROP_COUNT: Mutex<RefCell<u32>> = Mutex::new(RefCell::new(0));

#[interrupt]
fn EXTI4_15() {
    let exti = unsafe { &*pac::EXTI::ptr() };
    if exti.pr.read().pr12().bit_is_set() {
        // Clear first so another edge can pend while this bounded ISR exits.
        exti.pr.write(|w| w.pr12().set_bit());
        let clockwise = unsafe { (*pac::GPIOB::ptr()).idr.read().idr13().bit_is_clear() };
        cortex_m::interrupt::free(|cs| {
            let pushed = ENCODER_EVENTS
                .borrow(cs)
                .borrow_mut()
                .push_back(EncoderEvent {
                    direction: if clockwise { 1 } else { -1 },
                    tick: monotonic_ms(),
                })
                .is_ok();
            let mut edges = ENCODER_EDGE_COUNT.borrow(cs).borrow_mut();
            *edges = edges.wrapping_add(1);
            if !pushed {
                let mut drops = ENCODER_DROP_COUNT.borrow(cs).borrow_mut();
                *drops = drops.wrapping_add(1);
            }
        });
    }
}

pub(crate) fn encoder_counts() -> (u32, u32) {
    cortex_m::interrupt::free(|cs| {
        (
            *ENCODER_EDGE_COUNT.borrow(cs).borrow(),
            *ENCODER_DROP_COUNT.borrow(cs).borrow(),
        )
    })
}

pub(crate) fn take_encoder_adjustment(
    last_tick: &mut u16,
    last_direction: &mut i8,
    velocity: &mut u8,
) -> (i8, i8) {
    cortex_m::interrupt::free(|cs| {
        let mut queue = ENCODER_EVENTS.borrow(cs).borrow_mut();
        let mut raw = 0i16;
        let mut accelerated = 0i16;
        while let Some(event) = queue.pop_front() {
            let elapsed = event.tick.wrapping_sub(*last_tick);
            if event.direction != *last_direction || elapsed > ENCODER_ACCELERATION_IDLE_MS {
                *velocity = 1;
            } else {
                *velocity = velocity.saturating_add(1).min(16);
            }
            *last_tick = event.tick;
            *last_direction = event.direction;
            let multiplier: i16 = match *velocity {
                0 | 1 => 1,
                2..=3 => 2,
                4..=5 => 4,
                6..=8 => 8,
                _ => 16,
            };
            raw = raw.saturating_add(i16::from(event.direction));
            accelerated = accelerated.saturating_add(i16::from(event.direction) * multiplier);
        }
        (
            raw.clamp(i16::from(i8::MIN), i16::from(i8::MAX)) as i8,
            accelerated.clamp(i16::from(i8::MIN), i16::from(i8::MAX)) as i8,
        )
    })
}

pub(crate) fn monotonic_ms() -> u16 {
    unsafe { (*pac::TIM3::ptr()).cnt.read().cnt().bits() }
}
