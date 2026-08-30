#![no_main]
#![no_std]

//! The frozen page-0 trampoline. Written once per device, never updated.
//! Everything here must stay tiny and versionless: sense the interlock,
//! read the slot flag, sanity-check the candidate vectors, jump.

use boot_shared::{image, layout, trampoline_decision, BootTarget};
use core::ptr::{read_volatile, write_volatile};

/// Encoder switch PB14, active low (external pull-up). Two samples ~1 ms
/// apart must both read pressed — the interlock override to golden.
fn interlock_held() -> bool {
    unsafe {
        let ahbenr = 0x4002_1014usize as *mut u32;
        write_volatile(ahbenr, read_volatile(ahbenr) | (1 << 18));
        let idr = 0x4800_0410usize as *const u32;
        let first = read_volatile(idr) & (1 << 14) == 0;
        for _ in 0..8_000 {
            core::hint::spin_loop();
        }
        let second = read_volatile(idr) & (1 << 14) == 0;
        first && second
    }
}

fn vectors_at(base: u32, size: u32) -> bool {
    let check = image::VectorCheck {
        initial_sp: unsafe { read_volatile(base as *const u32) },
        reset_vector: unsafe { read_volatile((base + 4) as *const u32) },
    };
    image::vectors_valid(check, base, size)
}

/// Emulate the hardware boot for a slot: MSP from its vector 0, jump to its
/// reset vector.
fn enter(base: u32) -> ! {
    unsafe {
        let stack = read_volatile(base as *const u32);
        let entry = read_volatile((base + 4) as *const u32);
        core::arch::asm!(
            "msr msp, {stack}",
            "bx {entry}",
            stack = in(reg) stack,
            entry = in(reg) entry | 1,
            options(noreturn),
        );
    }
}

#[cortex_m_rt::entry]
fn main() -> ! {
    let flag = unsafe {
        read_volatile((layout::METADATA_ADDR + 4) as *const u32)
    };
    let target = trampoline_decision(
        flag,
        interlock_held(),
        vectors_at(layout::SLOT_A_BASE, layout::SLOT_A_SIZE),
        vectors_at(layout::SLOT_B_BASE, layout::SLOT_B_SIZE),
        vectors_at(layout::LEGACY_APP_BASE, layout::LEGACY_APP_MAX_SIZE),
    );
    match target {
        BootTarget::Golden => enter(layout::SLOT_A_BASE),
        BootTarget::SlotB => enter(layout::SLOT_B_BASE),
        BootTarget::Legacy => enter(layout::LEGACY_APP_BASE),
        BootTarget::Halt => loop {
            core::hint::spin_loop();
        },
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
