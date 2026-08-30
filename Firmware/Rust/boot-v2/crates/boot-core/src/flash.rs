//! Flash driver for the v2 boot cores (RM0360 §5): half-word programming,
//! KEYR unlock, PER/PG/STRT, SR polling. Offsets cross-checked against the
//! application's proven journal driver (`benchvolt-pd/src/boot.rs`).

#![allow(dead_code)]

use core::ptr::{read_volatile, write_volatile};

const FLASH_BASE: usize = 0x4002_2000;
const KEYR: *mut u32 = (FLASH_BASE + 0x04) as *mut u32;
const SR: *mut u32 = (FLASH_BASE + 0x0C) as *mut u32;
const CR: *mut u32 = (FLASH_BASE + 0x10) as *mut u32;
const AR: *mut u32 = (FLASH_BASE + 0x14) as *mut u32;

const SR_ERRORS: u32 = (1 << 2) | (1 << 4); // PGERR | WRPERR
const SR_EOP: u32 = 1 << 5;
const CR_PG: u32 = 1 << 0;
const CR_PER: u32 = 1 << 1;
const CR_STRT: u32 = 1 << 6;
const CR_LOCK: u32 = 1 << 7;
const KEYR1: u32 = 0x4567_0123;
const KEYR2: u32 = 0xCDEF_89AB;

pub const PAGE_SIZE: usize = 2_048;

/// Bounded flash-ready wait. Must be linked into RAM-resident code by the
/// owning image (`.data.flash_wait`), mirroring the application's
/// `benchvolt_wait_for_flash_ready`.
#[inline(never)]
#[link_section = ".data.flash_wait"]
pub fn wait_ready() -> bool {
    for _ in 0..2_000_000 {
        if unsafe { read_volatile(SR) } & 1 == 0 {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

fn unlock() {
    unsafe {
        if !wait_ready() {
            return;
        }
        if read_volatile(CR) & CR_LOCK != 0 {
            write_volatile(KEYR, KEYR1);
            write_volatile(KEYR, KEYR2);
        }
    }
}

fn relock() {
    unsafe {
        write_volatile(CR, read_volatile(CR) | CR_LOCK);
    }
}

/// Erase one 2 KiB page; verifies blank afterwards.
pub fn erase_page(address: usize) -> bool {
    unsafe {
        if !wait_ready() {
            return false;
        }
        unlock();
        write_volatile(SR, SR_EOP | SR_ERRORS);
        write_volatile(CR, read_volatile(CR) | CR_PER);
        write_volatile(AR, address as u32);
        write_volatile(CR, read_volatile(CR) | CR_STRT);
        let ok = wait_ready() && read_volatile(SR) & SR_ERRORS == 0;
        // PER must be cleared before any later PG — LOCK preserves CR bits.
        write_volatile(CR, read_volatile(CR) & !CR_PER);
        relock();
        ok && blank_check(address)
    }
}

/// Erase `count` pages starting at `address` (caller validates the range).
pub fn erase_pages(address: usize, count: usize) -> bool {
    for page in 0..count {
        if !erase_page(address + page * PAGE_SIZE) {
            return false;
        }
    }
    true
}

/// Program one word at an even address (two half-word writes, the pattern
/// proven by the application's journal driver). Verifies by read-back.
pub fn program_word(address: usize, value: u32) -> bool {
    unsafe {
        if !wait_ready() {
            return false;
        }
        unlock();
        write_volatile(SR, SR_EOP | SR_ERRORS);
        write_volatile(CR, read_volatile(CR) | CR_PG);
        write_volatile(address as *mut u16, (value & 0xFFFF) as u16);
        let first = wait_ready();
        if first {
            write_volatile((address + 2) as *mut u16, (value >> 16) as u16);
        }
        let done = first && wait_ready();
        let sr = read_volatile(SR);
        let ok = done && sr & SR_ERRORS == 0;
        // PG must not leak into the next operation — LOCK preserves CR bits.
        write_volatile(CR, read_volatile(CR) & !CR_PG);
        relock();
        let verified = ok && read_volatile(address as *const u32) == value;
        if !verified {
            // Bring-up diagnostics: last failed program (site 1).
            write_volatile(0x2000_0148usize as *mut u32, address as u32);
            write_volatile(0x2000_014Cusize as *mut u32, sr);
            write_volatile(
                0x2000_0150usize as *mut u32,
                0x0001_0000 | (!first as u32) << 1 | !ok as u32,
            );
        }
        verified
    }
}

fn blank_check(address: usize) -> bool {
    (0..PAGE_SIZE).all(|offset| unsafe { read_volatile((address + offset) as *const u8) } == 0xFF)
}

/// True when every byte in the range reads erased (0xFF).
pub fn range_is_blank(address: usize, len: usize) -> bool {
    (0..len).all(|offset| unsafe { read_volatile((address + offset) as *const u8) } == 0xFF)
}
