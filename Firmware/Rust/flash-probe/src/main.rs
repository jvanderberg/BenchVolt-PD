#![no_main]
#![no_std]

use core::ptr::{read_volatile, write_volatile};
use cortex_m_rt::entry;

// Flash register block, mirroring the offsets already proven by the
// application's settings journal (benchvolt-pd/src/boot.rs).
const FLASH_BASE: usize = 0x4002_2000;
const KEYR: *mut u32 = (FLASH_BASE + 0x04) as *mut u32;
const SR: *mut u32 = (FLASH_BASE + 0x0c) as *mut u32;
const CR: *mut u32 = (FLASH_BASE + 0x10) as *mut u32;
const AR: *mut u32 = (FLASH_BASE + 0x14) as *mut u32;

const SR_BSY: u32 = 1;
const SR_ERRORS: u32 = (1 << 2) | (1 << 4); // PGERR | WRPERR
const SR_EOP: u32 = 1 << 5;
const CR_PG: u32 = 1 << 0;
const CR_PER: u32 = 1 << 1;
const CR_STRT: u32 = 1 << 6;
const CR_LOCK: u32 = 1 << 7;

// Scratch page 32 (0x08010000): inside the old application region, far from
// the settings/metadata pages and the bootloader. Erasing it only invalidates
// the installed app's CRC, which puts the stock bootloader back into updater
// mode; reflash the normal application afterwards. The bootloader itself is
// never touched by this probe.
const SCRATCH: usize = 0x0801_0000;
const PAGE_BYTES: usize = 2_048;
const PAGE_WORDS: usize = PAGE_BYTES / 4;
const CYCLES: u32 = 100;

// Result block, read back at a fixed SRAM address after the run. Progress
// fields are written volatile-ly so a debugger can watch the run live.
const RESULTS_ADDR: usize = 0x2000_2000;
const OFF_MAGIC: usize = 0x00;
const OFF_DONE: usize = 0x04;
const OFF_PASS: usize = 0x08;
const OFF_CYCLES: usize = 0x0c;
const OFF_WORD_PROGRAMS: usize = 0x10;
const OFF_REPROGRAMS: usize = 0x14;
const OFF_MISMATCHES: usize = 0x18;
const OFF_ERR_KIND: usize = 0x1c; // 0 ok, 1 erase, 2 prog timeout, 3 mismatch, 4 blank-check
const OFF_ERR_WORD: usize = 0x20;
const OFF_ERR_STEP: usize = 0x24;
const OFF_ERR_EXPECTED: usize = 0x28;
const OFF_ERR_ACTUAL: usize = 0x2c;
const MAGIC: u32 = 0x4650_5632; // "V2PF"

fn set_u32(offset: usize, value: u32) {
    unsafe { write_volatile((RESULTS_ADDR + offset) as *mut u32, value) };
}

// PB8 (red status LED), direct register pokes, no HAL.
const RCC_AHBENR: *mut u32 = 0x4000_6C14 as *mut u32;
const GPIOB_MODER: *mut u32 = 0x4800_0400 as *mut u32;
const GPIOB_BSRR: *mut u32 = 0x4800_0418 as *mut u32;

fn led_init() {
    unsafe {
        write_volatile(RCC_AHBENR, read_volatile(RCC_AHBENR) | (1 << 18));
        let moder = read_volatile(GPIOB_MODER);
        write_volatile(GPIOB_MODER, (moder & !(0b11 << 16)) | (0b01 << 16));
    }
}

fn led_set(on: bool) {
    unsafe { write_volatile(GPIOB_BSRR, if on { 1 << 8 } else { 1 << (8 + 16) }) };
}

// Kept in the .data (RAM) output section so the bounded timeout stays
// meaningful while the flash controller stalls instruction fetch, exactly
// like the application's benchvolt_wait_for_flash_ready.
#[inline(never)]
#[link_section = ".data.flash_wait"]
fn wait_ready() -> bool {
    let mut spins: u32 = 0;
    while spins < 2_000_000 {
        if unsafe { read_volatile(SR) } & SR_BSY == 0 {
            return true;
        }
        spins += 1;
        core::hint::spin_loop();
    }
    false
}

fn flash_unlock() {
    unsafe {
        if read_volatile(CR) & CR_LOCK != 0 {
            write_volatile(KEYR, 0x4567_0123);
            write_volatile(KEYR, 0xcdef_89ab);
        }
    }
}

// 0 = ok, 1 = erase failure, 4 = blank-check failure
fn erase_page(address: usize) -> u32 {
    unsafe {
        if !wait_ready() {
            return 1;
        }
        write_volatile(SR, SR_EOP | SR_ERRORS);
        write_volatile(CR, read_volatile(CR) | CR_PER);
        write_volatile(AR, address as u32);
        write_volatile(CR, read_volatile(CR) | CR_STRT);
        let ok = wait_ready()
            && read_volatile(SR) & SR_ERRORS == 0
            && (0..PAGE_BYTES)
                .all(|offset| read_volatile((address + offset) as *const u8) == 0xff);
        write_volatile(CR, read_volatile(CR) & !CR_PER);
        if !ok {
            4
        } else {
            0
        }
    }
}

// 0 = ok, 2 = timeout, 3 = flash error flag. Caller does the read-back.
fn program_word(address: usize, value: u32) -> u32 {
    unsafe {
        if !wait_ready() {
            return 2;
        }
        write_volatile(SR, SR_EOP | SR_ERRORS);
        write_volatile(CR, read_volatile(CR) | CR_PG);
        write_volatile((address) as *mut u16, (value & 0xffff) as u16);
        if !wait_ready() {
            return 2;
        }
        write_volatile((address + 2) as *mut u16, (value >> 16) as u16);
        if !wait_ready() {
            return 2;
        }
        write_volatile(CR, read_volatile(CR) & !CR_PG);
        let errors = read_volatile(SR) & SR_ERRORS;
        if errors != 0 {
            3
        } else {
            0
        }
    }
}

fn record_error(kind: u32, word: u32, step: u32, expected: u32, actual: u32) {
    set_u32(OFF_ERR_KIND, kind);
    set_u32(OFF_ERR_WORD, word);
    set_u32(OFF_ERR_STEP, step);
    set_u32(OFF_ERR_EXPECTED, expected);
    set_u32(OFF_ERR_ACTUAL, actual);
}

#[entry]
fn main() -> ! {
    led_init();
    set_u32(OFF_MAGIC, MAGIC);
    set_u32(OFF_DONE, 0);
    set_u32(OFF_PASS, 0);
    set_u32(OFF_ERR_KIND, 0);

    flash_unlock();

    let mut mismatches: u32 = 0;
    let mut word_programs: u32 = 0;
    let mut reprograms: u32 = 0;
    let mut cycles_done: u32 = 0;
    let mut aborted = false;

    for _cycle in 0..CYCLES {
        match erase_page(SCRATCH) {
            0 => {}
            kind => {
                record_error(kind, 0xffff_ffff, 0, 0, 0);
                aborted = true;
                break;
            }
        }
        for w in 0..PAGE_WORDS {
            let address = SCRATCH + w * 4;
            for step in 1..=32u32 {
                let expected = if step == 32 { 0 } else { 0xffff_ffff << step };
                match program_word(address, expected) {
                    0 => {}
                    3 => {
                        record_error(3, w as u32, step, expected, unsafe { read_volatile(SR) });
                        aborted = true;
                        break;
                    }
                    _ => {
                        record_error(2, w as u32, step, expected, 0);
                        aborted = true;
                        break;
                    }
                }
                word_programs += 1;
                if step > 1 {
                    reprograms += 1;
                }
                let actual = unsafe { read_volatile(address as *const u32) };
                if actual != expected {
                    mismatches += 1;
                    if mismatches == 1 {
                        record_error(3, w as u32, step, expected, actual);
                    }
                }
            }
            if aborted {
                break;
            }
            if w % 64 == 0 {
                led_set((w / 64) & 1 == 0);
                set_u32(OFF_CYCLES, cycles_done);
                set_u32(OFF_WORD_PROGRAMS, word_programs);
                set_u32(OFF_REPROGRAMS, reprograms);
                set_u32(OFF_MISMATCHES, mismatches);
            }
        }
        if aborted {
            break;
        }
        cycles_done += 1;
        set_u32(OFF_CYCLES, cycles_done);
    }

    unsafe { write_volatile(CR, read_volatile(CR) | CR_LOCK) };

    let pass = !aborted && mismatches == 0 && cycles_done == CYCLES;
    set_u32(OFF_PASS, pass as u32);
    set_u32(OFF_DONE, 1);

    if pass {
        led_set(true);
        loop {
            core::hint::spin_loop();
        }
    } else {
        loop {
            led_set(true);
            for _ in 0..200_000 {
                core::hint::spin_loop();
            }
            led_set(false);
            for _ in 0..200_000 {
                core::hint::spin_loop();
            }
            led_set(true);
            for _ in 0..200_000 {
                core::hint::spin_loop();
            }
            led_set(false);
            for _ in 0..800_000 {
                core::hint::spin_loop();
            }
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    led_set(false);
    loop {
        core::hint::spin_loop();
    }
}
