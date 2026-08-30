#![no_main]
#![no_std]

use boot_core::service::feed_iwdg;
use boot_usb::{Event, Usb, MAX_PACKET};

/// Test payload for the v2 app partition: proves the launch path ran (SRAM
/// marker + USB identity "BV2A") and refuses every flash command — a live
/// application must never erase the partition it is executing from; real
/// updates go through JUMP:BOOTLOADER.
#[cortex_m_rt::entry]
fn main() -> ! {
    unsafe { core::ptr::write_volatile(0x2000_0144usize as *mut u32, 0xA221_57B0) };
    let mut usb = Usb::new();
    let mut packet = [0u8; MAX_PACKET];
    loop {
        feed_iwdg();
        if usb.poll() == Event::BulkRx {
            let count = usb.recv(&mut packet);
            if count != 0 {
                if packet[0] == boot_core::protocol::CMD_INFO {
                    let mut info = boot_core::protocol::cmd_info();
                    info[0..4].copy_from_slice(b"BV2A");
                    usb.send(&info);
                } else {
                    usb.send(&[boot_core::protocol::NACK]);
                }
            }
            usb.rx_release();
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
