use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};

use benchvolt_pd::reset_cause::ResetReason;

static LAST_HW_OPERATION: AtomicU8 = AtomicU8::new(0);
static LAST_HW_ERROR: AtomicU8 = AtomicU8::new(0);
static RESET_CAUSES: AtomicU8 = AtomicU8::new(0);
static RESET_REASON: AtomicU8 = AtomicU8::new(0);
static HW_RETRY_COUNT: AtomicU32 = AtomicU32::new(0);
static CH5_TPS_STATUS: AtomicU8 = AtomicU8::new(0);

#[inline(always)]
pub(crate) fn record_hw_retries(count: u32) {
    let current = HW_RETRY_COUNT.load(Ordering::Relaxed);
    HW_RETRY_COUNT.store(current.saturating_add(count), Ordering::Relaxed);
}

static MAX_LOOP_GAP_TICKS: AtomicU8 = AtomicU8::new(0);

#[inline(always)]
pub(crate) fn record_loop_gap(ticks: u16) {
    let clamped = ticks.min(255) as u8;
    if clamped > MAX_LOOP_GAP_TICKS.load(Ordering::Relaxed) {
        MAX_LOOP_GAP_TICKS.store(clamped, Ordering::Relaxed);
    }
}

#[inline(always)]
pub(crate) fn take_loop_gap() -> u8 {
    let value = MAX_LOOP_GAP_TICKS.load(Ordering::Relaxed);
    MAX_LOOP_GAP_TICKS.store(0, Ordering::Relaxed);
    value
}

#[inline(always)]
pub(crate) fn record_hw_error(operation: u8, error: u8) {
    LAST_HW_OPERATION.store(operation, Ordering::Relaxed);
    LAST_HW_ERROR.store(error, Ordering::Relaxed);
}

#[inline(always)]
pub(crate) fn record_reset(causes: u8, reason: Option<ResetReason>) {
    RESET_CAUSES.store(causes, Ordering::Relaxed);
    RESET_REASON.store(reason.map_or(0, |value| value as u8), Ordering::Relaxed);
}

#[inline(always)]
pub(crate) fn record_ch5_tps_status(status: u8) {
    CH5_TPS_STATUS.store(status, Ordering::Relaxed);
}

#[inline(always)]
pub(crate) fn last_hw_operation() -> u8 {
    LAST_HW_OPERATION.load(Ordering::Relaxed)
}

#[inline(always)]
pub(crate) fn last_hw_error() -> u8 {
    LAST_HW_ERROR.load(Ordering::Relaxed)
}

#[inline(always)]
pub(crate) fn hw_retry_count() -> u32 {
    HW_RETRY_COUNT.load(Ordering::Relaxed)
}

#[inline(always)]
pub(crate) fn reset_causes() -> u8 {
    RESET_CAUSES.load(Ordering::Relaxed)
}

#[inline(always)]
pub(crate) fn reset_reason() -> u8 {
    RESET_REASON.load(Ordering::Relaxed)
}

#[inline(always)]
pub(crate) fn ch5_tps_status() -> u8 {
    CH5_TPS_STATUS.load(Ordering::Relaxed)
}
