use core::cell::RefCell;

use cortex_m::interrupt::Mutex;
use heapless::Deque;
use stm32f0xx_hal::pac::{self, interrupt};

use benchvolt_poc::input_policy::{clamp_adjustment, encoder_direction, EncoderAccumulator};

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
        let dt_high = unsafe { (*pac::GPIOB::ptr()).idr.read().idr13().bit_is_set() };
        cortex_m::interrupt::free(|cs| {
            let pushed = ENCODER_EVENTS
                .borrow(cs)
                .borrow_mut()
                .push_back(EncoderEvent {
                    direction: encoder_direction(dt_high),
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

pub(crate) fn take_encoder_adjustment(accumulator: &mut EncoderAccumulator) -> (i8, i8) {
    cortex_m::interrupt::free(|cs| {
        let mut queue = ENCODER_EVENTS.borrow(cs).borrow_mut();
        let mut raw = 0i16;
        let mut accelerated = 0i16;
        while let Some(event) = queue.pop_front() {
            let (event_raw, event_accelerated) = accumulator.step(event.direction, event.tick);
            raw = raw.saturating_add(event_raw);
            accelerated = accelerated.saturating_add(event_accelerated);
        }
        clamp_adjustment(raw, accelerated)
    })
}

pub(crate) fn monotonic_ms() -> u16 {
    unsafe { (*pac::TIM3::ptr()).cnt.read().cnt().bits() }
}
