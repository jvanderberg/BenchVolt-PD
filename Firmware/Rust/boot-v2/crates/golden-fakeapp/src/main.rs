#![no_main]
#![no_std]

use boot_core::service::run_boot_core;

/// The golden core linked at 0x08008000 as an ordinary v1 application
/// image, for the migration proof through the stock bootloader.
#[cortex_m_rt::entry]
fn main() -> ! {
    run_boot_core(true)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
