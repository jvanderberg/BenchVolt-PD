#![no_main]
#![no_std]

use boot_core::service::boot_core_entry;

/// Slot B ("working"): the updatable core. Never writes core sections —
/// the running core must not touch its own pages; slot-B updates are
/// golden's job.
#[cortex_m_rt::entry]
fn main() -> ! {
    boot_core_entry(false, 0x3B)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
