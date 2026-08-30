#![no_main]
#![no_std]

use boot_core::service::boot_core_entry;

/// Slot A ("golden"): recovery core. Core-section writes require the
/// physical interlock; the launch/updater decision is shared boot-core
/// logic.
#[cortex_m_rt::entry]
fn main() -> ! {
    boot_core_entry(true, 0x601D, || {})
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
