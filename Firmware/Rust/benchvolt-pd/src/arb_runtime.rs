use core::{
    cell::RefCell,
    sync::atomic::{AtomicU32, Ordering},
};

use benchvolt_pd::arb::{Buffer, DataChunk, SchedulerStatus, Start};
use cortex_m::interrupt::Mutex;

static BUFFER: Mutex<RefCell<Buffer>> = Mutex::new(RefCell::new(Buffer::new()));
static INDEX: AtomicU32 = AtomicU32::new(0);
static CYCLES: AtomicU32 = AtomicU32::new(0);
static LATE_UPDATES: AtomicU32 = AtomicU32::new(0);
static SKIPPED_CYCLES: AtomicU32 = AtomicU32::new(0);

pub(crate) fn write(chunk: DataChunk) {
    cortex_m::interrupt::free(|cs| BUFFER.borrow(cs).borrow_mut().write(chunk));
}

pub(crate) fn validate(start: Start) -> Option<(u16, u16, u16)> {
    cortex_m::interrupt::free(|cs| BUFFER.borrow(cs).borrow().validate(start))
}

pub(crate) fn with_buffer<R>(operation: impl FnOnce(&Buffer) -> R) -> R {
    cortex_m::interrupt::free(|cs| operation(&BUFFER.borrow(cs).borrow()))
}

pub(crate) fn reset_status() {
    INDEX.store(0, Ordering::Relaxed);
    CYCLES.store(0, Ordering::Relaxed);
    LATE_UPDATES.store(0, Ordering::Relaxed);
    SKIPPED_CYCLES.store(0, Ordering::Relaxed);
}

pub(crate) fn update_status(status: SchedulerStatus) {
    INDEX.store(u32::from(status.index), Ordering::Relaxed);
    CYCLES.store(status.cycles, Ordering::Relaxed);
    LATE_UPDATES.store(status.late_updates, Ordering::Relaxed);
    SKIPPED_CYCLES.store(status.skipped_cycles, Ordering::Relaxed);
}

pub(crate) fn status() -> (u32, u32, u32, u32) {
    (
        INDEX.load(Ordering::Relaxed),
        CYCLES.load(Ordering::Relaxed),
        LATE_UPDATES.load(Ordering::Relaxed),
        SKIPPED_CYCLES.load(Ordering::Relaxed),
    )
}
