#![no_main]
#![no_std]

// Not yet implemented (plan milestone M1/M2); minimal valid image so the
// workspace builds.
#[cortex_m_rt::entry]
fn main() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
