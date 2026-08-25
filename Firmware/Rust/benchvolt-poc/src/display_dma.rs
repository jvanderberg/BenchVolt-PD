use core::{
    cell::RefCell,
    convert::Infallible,
    sync::atomic::{AtomicBool, AtomicU16, AtomicU8, Ordering},
};

use cortex_m::interrupt::Mutex;
use embedded_graphics::{
    pixelcolor::Rgb565,
    prelude::{DrawTarget, IntoStorage, OriginDimensions, Pixel, Point, Size},
    primitives::Rectangle,
};

use benchvolt_poc::paint_queue::{dma_deadline_reached, PaintCommand, PaintQueue, PaintTransfer};
use stm32f0xx_hal::pac::{self, interrupt};

use crate::input::monotonic_ms;

const DISPLAY_SIZE: Size = Size::new(320, 170);
const COMMAND_CAPACITY: usize = 192;

static COMMANDS: Mutex<RefCell<PaintQueue<COMMAND_CAPACITY>>> =
    Mutex::new(RefCell::new(PaintQueue::new()));
static OVERFLOWED: AtomicBool = AtomicBool::new(false);
static FAILED: AtomicBool = AtomicBool::new(false);
static INSTALLED: AtomicBool = AtomicBool::new(false);
static HIGH_WATER: AtomicU16 = AtomicU16::new(0);
static LIFECYCLE: AtomicU8 = AtomicU8::new(LIFECYCLE_UNINSTALLED);

const LIFECYCLE_UNINSTALLED: u8 = 0;
const LIFECYCLE_INSTALLED: u8 = 1;
const LIFECYCLE_SELF_TEST: u8 = 2;
const LIFECYCLE_AWAIT_CONTINUE: u8 = 3;
const LIFECYCLE_FULL_ENQUEUE: u8 = 4;
const LIFECYCLE_FULL_DRAIN: u8 = 5;
const LIFECYCLE_HEALTHY: u8 = 6;
const LIFECYCLE_FAILED: u8 = u8::MAX;
const TRANSFER_TIMEOUT_MS: u16 = 100;
const ALLOW_BOOT_SEAL: bool = true;

const RCC_AHBENR: *mut u32 = 0x4002_1014 as *mut u32;
const DMA_ISR: *const u32 = 0x4002_0000 as *const u32;
const DMA_IFCR: *mut u32 = 0x4002_0004 as *mut u32;
const DMA_CH3_CCR: *mut u32 = 0x4002_0030 as *mut u32;
const DMA_CH3_CNDTR: *mut u32 = 0x4002_0034 as *mut u32;
const DMA_CH3_CPAR: *mut u32 = 0x4002_0038 as *mut u32;
const DMA_CH3_CMAR: *mut u32 = 0x4002_003c as *mut u32;
const SPI1_CR2: *mut u32 = 0x4001_3004 as *mut u32;
const SPI1_SR: *const u32 = 0x4001_3008 as *const u32;
const SPI1_DR: u32 = 0x4001_300c;
const GPIOC_BSRR: *mut u32 = 0x4800_0818 as *mut u32;
const GPIOD_BSRR: *mut u32 = 0x4800_0c18 as *mut u32;

const DMA_CH3_FLAGS: u32 = 0x0f << 8;
const DMA_CH3_TRANSFER_COMPLETE: u32 = 1 << 9;
const DMA_CH3_TRANSFER_ERROR: u32 = 1 << 11;
const DMA_CCR_BYTE_CONFIG: u32 = (1 << 1) | (1 << 3) | (1 << 4) | (1 << 7) | (1 << 12);
const SPI_TX_DMA_ENABLE: u32 = 1 << 1;
const SPI_TX_FIFO_LEVEL: u32 = 0b11 << 11;
const SPI_BUSY: u32 = 1 << 7;
const DC_PIN: u32 = 10;
const CS_PIN: u32 = 2;
const DISPLAY_Y_OFFSET: u16 = 35;
const PIXEL_BUFFER_BYTES: usize = 256;

struct DmaState {
    active: Option<PaintCommand>,
    step: u8,
    pixels_remaining: u32,
    setup: [u8; 4],
    pixels: [u8; PIXEL_BUFFER_BYTES],
    deadline: u16,
}

impl DmaState {
    const fn new() -> Self {
        Self {
            active: None,
            step: 0,
            pixels_remaining: 0,
            setup: [0; 4],
            pixels: [0; PIXEL_BUFFER_BYTES],
            deadline: 0,
        }
    }
}

static DMA_STATE: Mutex<RefCell<DmaState>> = Mutex::new(RefCell::new(DmaState::new()));

#[derive(Clone, Copy, Default)]
pub struct QueuedDisplay;

impl QueuedDisplay {
    pub const fn new() -> Self {
        Self
    }

    fn enqueue(command: PaintCommand) {
        match LIFECYCLE.load(Ordering::Acquire) {
            LIFECYCLE_INSTALLED | LIFECYCLE_SELF_TEST | LIFECYCLE_AWAIT_CONTINUE => return,
            LIFECYCLE_FULL_ENQUEUE | LIFECYCLE_FULL_DRAIN | LIFECYCLE_HEALTHY => {}
            _ => {
                fail_without_hardware();
                return;
            }
        }
        enqueue_unchecked(command);
    }
}

fn enqueue_unchecked(command: PaintCommand) {
    let deadline = monotonic_ms().wrapping_add(TRANSFER_TIMEOUT_MS);
    loop {
        let queued = cortex_m::interrupt::free(|cs| {
            let mut queue = COMMANDS.borrow(cs).borrow_mut();
            queue.push(command).ok().map(|()| queue.len() as u16)
        });
        if let Some(queued) = queued {
            if queued > HIGH_WATER.load(Ordering::Relaxed) {
                HIGH_WATER.store(queued, Ordering::Relaxed);
            }
            service();
            return;
        }
        if FAILED.load(Ordering::Acquire) || dma_deadline_reached(monotonic_ms(), deadline) {
            OVERFLOWED.store(true, Ordering::Relaxed);
            abort_dma();
            return;
        }
        // A full display list applies backpressure without polling SPI. Keep
        // interrupts enabled so DMA and USB remain asynchronous, but do not
        // WFI here: TIM3 is a free-running counter without an update IRQ, so a
        // DMA stall with no USB host could otherwise sleep past this bounded
        // timeout and reach the watchdog instead.
        service();
        core::hint::spin_loop();
    }
}

impl OriginDimensions for QueuedDisplay {
    fn size(&self) -> Size {
        DISPLAY_SIZE
    }
}

impl DrawTarget for QueuedDisplay {
    type Color = Rgb565;
    type Error = Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        let mut span: Option<PaintCommand> = None;
        for Pixel(point, color) in pixels {
            if point.x < 0
                || point.y < 0
                || point.x >= DISPLAY_SIZE.width as i32
                || point.y >= DISPLAY_SIZE.height as i32
            {
                continue;
            }
            let pixel = PaintCommand {
                x: point.x as u16,
                y: point.y as u16,
                width: 1,
                height: 1,
                color: color.into_storage(),
            };
            if let Some(mut current) = span {
                if current.y == pixel.y
                    && current.color == pixel.color
                    && u32::from(current.x) + u32::from(current.width) == u32::from(pixel.x)
                {
                    current.width += 1;
                    span = Some(current);
                    continue;
                }
                Self::enqueue(current);
            }
            span = Some(pixel);
        }
        if let Some(span) = span {
            Self::enqueue(span);
        }
        Ok(())
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        let clipped = area.intersection(&Rectangle::new(Point::zero(), DISPLAY_SIZE));
        if clipped.size.width != 0 && clipped.size.height != 0 {
            Self::enqueue(PaintCommand {
                x: clipped.top_left.x as u16,
                y: clipped.top_left.y as u16,
                width: clipped.size.width as u16,
                height: clipped.size.height as u16,
                color: color.into_storage(),
            });
        }
        Ok(())
    }
}

/// Takes ownership of the initialized display resources. The panel is set up
/// with the blocking driver exactly once; every subsequent byte uses SPI1 TX
/// DMA channel 3.
pub fn install<T>(resources: T) {
    core::mem::forget(resources);
    unsafe {
        RCC_AHBENR.write_volatile(RCC_AHBENR.read_volatile() | 1);
        DMA_CH3_CCR.write_volatile(0);
        // The stock bootloader may leave both DMA controller flags and the
        // shared channel-2/3 NVIC pending bit set across its jump.
        DMA_IFCR.write_volatile(u32::MAX);
        SPI1_CR2.write_volatile(SPI1_CR2.read_volatile() & !SPI_TX_DMA_ENABLE);
        set_cs(true);
        set_dc(true);
        cortex_m::peripheral::NVIC::unpend(pac::Interrupt::DMA1_CH2_3);
    }
    INSTALLED.store(true, Ordering::Release);
    LIFECYCLE.store(LIFECYCLE_INSTALLED, Ordering::Release);
    unsafe { cortex_m::peripheral::NVIC::unmask(pac::Interrupt::DMA1_CH2_3) };
}

pub fn begin_self_test() -> bool {
    if !transition_lifecycle(LIFECYCLE_INSTALLED, LIFECYCLE_SELF_TEST) {
        return false;
    }
    enqueue_unchecked(PaintCommand {
        x: 128,
        y: 53,
        width: 64,
        height: 64,
        color: 0xf81f,
    });
    true
}

pub fn begin_full_render() -> bool {
    transition_lifecycle(LIFECYCLE_AWAIT_CONTINUE, LIFECYCLE_FULL_ENQUEUE)
}

pub fn finish_full_render() {
    if transition_lifecycle(LIFECYCLE_FULL_ENQUEUE, LIFECYCLE_FULL_DRAIN) {
        service();
        mark_drain_complete_if_idle();
    }
}

pub fn ready_for_seal() -> bool {
    ALLOW_BOOT_SEAL && LIFECYCLE.load(Ordering::Acquire) == LIFECYCLE_HEALTHY
}

pub fn has_failed() -> bool {
    FAILED.load(Ordering::Acquire)
}

pub fn lifecycle_label() -> &'static str {
    match LIFECYCLE.load(Ordering::Acquire) {
        LIFECYCLE_UNINSTALLED => "UNINSTALLED",
        LIFECYCLE_INSTALLED => "INSTALLED",
        LIFECYCLE_SELF_TEST => "SELFTEST",
        LIFECYCLE_AWAIT_CONTINUE => "SELFTEST_OK",
        LIFECYCLE_FULL_ENQUEUE => "FULL_ENQUEUE",
        LIFECYCLE_FULL_DRAIN => "FULL_DRAIN",
        LIFECYCLE_HEALTHY => "HEALTHY",
        LIFECYCLE_FAILED => "FAILED",
        _ => "INVALID",
    }
}

pub fn service() {
    if !INSTALLED.load(Ordering::Acquire) || FAILED.load(Ordering::Relaxed) {
        return;
    }
    cortex_m::interrupt::free(|cs| {
        let mut state = DMA_STATE.borrow(cs).borrow_mut();
        if state.active.is_some() && dma_deadline_reached(monotonic_ms(), state.deadline) {
            fail_locked(&mut state, &mut COMMANDS.borrow(cs).borrow_mut());
        } else if state.active.is_none() {
            start_next(&mut state, &mut COMMANDS.borrow(cs).borrow_mut());
        }
    });
    mark_drain_complete_if_idle();
}

pub fn diagnostics() -> (usize, u16, bool, bool, bool) {
    let (queued, active) = cortex_m::interrupt::free(|cs| {
        (
            COMMANDS.borrow(cs).borrow().len(),
            DMA_STATE.borrow(cs).borrow().active.is_some(),
        )
    });
    (
        queued,
        HIGH_WATER.load(Ordering::Relaxed),
        active,
        OVERFLOWED.load(Ordering::Relaxed),
        FAILED.load(Ordering::Relaxed),
    )
}

#[interrupt]
fn DMA1_CH2_3() {
    let flags = unsafe { DMA_ISR.read_volatile() };
    if flags & (DMA_CH3_TRANSFER_COMPLETE | DMA_CH3_TRANSFER_ERROR) == 0 {
        return;
    }
    unsafe {
        DMA_CH3_CCR.write_volatile(0);
        SPI1_CR2.write_volatile(SPI1_CR2.read_volatile() & !SPI_TX_DMA_ENABLE);
        DMA_IFCR.write_volatile(DMA_CH3_FLAGS);
    }
    if flags & DMA_CH3_TRANSFER_ERROR != 0 || !wait_spi_idle() {
        abort_dma();
        return;
    }
    cortex_m::interrupt::free(|cs| {
        let mut state = DMA_STATE.borrow(cs).borrow_mut();
        advance(&mut state, &mut COMMANDS.borrow(cs).borrow_mut());
    });
}

fn start_next(state: &mut DmaState, queue: &mut PaintQueue<COMMAND_CAPACITY>) {
    let Some(command) = queue.pop() else {
        return;
    };
    state.active = Some(command);
    state.step = 0;
    unsafe { set_cs(false) };
    start_step(state, command.transfer(0, DISPLAY_Y_OFFSET));
}

fn advance(state: &mut DmaState, queue: &mut PaintQueue<COMMAND_CAPACITY>) {
    let Some(command) = state.active else {
        fail_locked(state, queue);
        return;
    };
    if state.step == 5 && state.pixels_remaining != 0 {
        start_pixel_chunk(state);
        return;
    }
    state.step = state.step.saturating_add(1);
    let transfer = command.transfer(state.step, DISPLAY_Y_OFFSET);
    if transfer == PaintTransfer::Complete {
        unsafe { set_cs(true) };
        state.active = None;
        state.step = 0;
        start_next(state, queue);
    } else {
        start_step(state, transfer);
    }
}

fn start_step(state: &mut DmaState, transfer: PaintTransfer) {
    state.deadline = monotonic_ms().wrapping_add(TRANSFER_TIMEOUT_MS);
    match transfer {
        PaintTransfer::Bytes {
            data_mode,
            bytes,
            len,
        } => {
            state.setup = bytes;
            unsafe {
                set_dc(data_mode);
                start_byte_transfer(state.setup.as_ptr(), usize::from(len));
            }
        }
        PaintTransfer::RepeatedColor { color, pixels } => {
            let bytes = color.to_be_bytes();
            for pixel in state.pixels.chunks_exact_mut(2) {
                pixel.copy_from_slice(&bytes);
            }
            state.pixels_remaining = pixels;
            unsafe { set_dc(true) };
            start_pixel_chunk(state);
        }
        PaintTransfer::Complete => {}
    }
}

unsafe fn start_byte_transfer(bytes: *const u8, length: usize) {
    start_transfer(bytes.cast(), length, DMA_CCR_BYTE_CONFIG);
}

fn start_pixel_chunk(state: &mut DmaState) {
    let pixels = state.pixels_remaining.min((PIXEL_BUFFER_BYTES / 2) as u32) as usize;
    state.pixels_remaining -= pixels as u32;
    state.deadline = monotonic_ms().wrapping_add(TRANSFER_TIMEOUT_MS);
    unsafe { start_byte_transfer(state.pixels.as_ptr(), pixels * 2) };
}

unsafe fn start_transfer(memory: *const u8, length: usize, config: u32) {
    DMA_CH3_CCR.write_volatile(0);
    SPI1_CR2.write_volatile(SPI1_CR2.read_volatile() & !SPI_TX_DMA_ENABLE);
    DMA_IFCR.write_volatile(DMA_CH3_FLAGS);
    DMA_CH3_CPAR.write_volatile(SPI1_DR);
    DMA_CH3_CMAR.write_volatile(memory as u32);
    DMA_CH3_CNDTR.write_volatile(length as u32);
    core::sync::atomic::compiler_fence(Ordering::Release);
    cortex_m::asm::dmb();
    DMA_CH3_CCR.write_volatile(config | 1);
    SPI1_CR2.write_volatile(SPI1_CR2.read_volatile() | SPI_TX_DMA_ENABLE);
}

fn wait_spi_idle() -> bool {
    // Match SPI_EndRxTxTransaction in the original C HAL. DMA TC only means
    // the final byte reached SPI_DR; TXE means the FIFO has room, not that it
    // is empty, and can race a one-byte ST7789 command before BSY asserts.
    let mut fifo_empty = false;
    for _ in 0..1_024 {
        if unsafe { SPI1_SR.read_volatile() } & SPI_TX_FIFO_LEVEL == 0 {
            fifo_empty = true;
            break;
        }
        core::hint::spin_loop();
    }
    if !fifo_empty {
        return false;
    }
    for _ in 0..1_024 {
        if unsafe { SPI1_SR.read_volatile() } & SPI_BUSY == 0 {
            cortex_m::asm::dmb();
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

fn abort_dma() {
    cortex_m::interrupt::free(|cs| {
        let mut state = DMA_STATE.borrow(cs).borrow_mut();
        fail_locked(&mut state, &mut COMMANDS.borrow(cs).borrow_mut());
    });
}

fn fail_without_hardware() {
    FAILED.store(true, Ordering::Relaxed);
    LIFECYCLE.store(LIFECYCLE_FAILED, Ordering::Release);
}

fn fail_locked(state: &mut DmaState, queue: &mut PaintQueue<COMMAND_CAPACITY>) {
    unsafe {
        DMA_CH3_CCR.write_volatile(0);
        SPI1_CR2.write_volatile(SPI1_CR2.read_volatile() & !SPI_TX_DMA_ENABLE);
        DMA_IFCR.write_volatile(DMA_CH3_FLAGS);
        set_cs(true);
    }
    state.active = None;
    state.step = 0;
    state.pixels_remaining = 0;
    queue.clear();
    FAILED.store(true, Ordering::Relaxed);
    LIFECYCLE.store(LIFECYCLE_FAILED, Ordering::Release);
}

fn mark_drain_complete_if_idle() {
    let idle = cortex_m::interrupt::free(|cs| {
        DMA_STATE.borrow(cs).borrow().active.is_none() && COMMANDS.borrow(cs).borrow().is_empty()
    });
    if !idle {
        return;
    }
    let lifecycle = LIFECYCLE.load(Ordering::Acquire);
    let next = match lifecycle {
        LIFECYCLE_SELF_TEST => LIFECYCLE_AWAIT_CONTINUE,
        LIFECYCLE_FULL_DRAIN => LIFECYCLE_HEALTHY,
        _ => return,
    };
    let _ = transition_lifecycle(lifecycle, next);
}

fn transition_lifecycle(from: u8, to: u8) -> bool {
    cortex_m::interrupt::free(|_| {
        if LIFECYCLE.load(Ordering::Acquire) != from {
            return false;
        }
        LIFECYCLE.store(to, Ordering::Release);
        true
    })
}

unsafe fn set_dc(high: bool) {
    GPIOC_BSRR.write_volatile(if high {
        1 << DC_PIN
    } else {
        1 << (DC_PIN + 16)
    });
}

unsafe fn set_cs(high: bool) {
    GPIOD_BSRR.write_volatile(if high {
        1 << CS_PIN
    } else {
        1 << (CS_PIN + 16)
    });
}
