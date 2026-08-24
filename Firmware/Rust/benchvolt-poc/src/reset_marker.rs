use core::mem::MaybeUninit;

use benchvolt_poc::reset_cause::{ResetMarker, ResetReason, MARKER_TAG};

#[link_section = ".uninit.reset_marker"]
static mut RESET_MARKER: MaybeUninit<ResetMarker> = MaybeUninit::uninit();

pub(crate) unsafe fn take(reset_causes: u8, ram_parity_disabled: bool) -> Option<ResetReason> {
    if !benchvolt_poc::reset_cause::retained_marker_read_allowed(reset_causes, ram_parity_disabled)
    {
        // Initialize the commit word without reading potentially untouched
        // parity-protected SRAM. Subcause reporting is best-effort in this mode.
        clear();
        return None;
    }
    let marker = core::ptr::read_volatile(core::ptr::addr_of!(RESET_MARKER).cast::<ResetMarker>());
    clear();
    marker.decode(reset_causes)
}

pub(crate) unsafe fn record(reason: ResetReason) {
    let marker = core::ptr::addr_of_mut!(RESET_MARKER).cast::<ResetMarker>();
    let code = MARKER_TAG | reason as u32;
    // Invalidate first and commit the code last so an interrupted write can
    // never be mistaken for a complete marker on the next boot.
    core::ptr::write_volatile(core::ptr::addr_of_mut!((*marker).code), 0);
    core::ptr::write_volatile(core::ptr::addr_of_mut!((*marker).inverse), !code);
    cortex_m::asm::dsb();
    core::ptr::write_volatile(core::ptr::addr_of_mut!((*marker).code), code);
    cortex_m::asm::dsb();
}

pub(crate) unsafe fn clear() {
    core::ptr::write_volatile(core::ptr::addr_of_mut!(RESET_MARKER).cast::<u32>(), 0);
    cortex_m::asm::dsb();
}
