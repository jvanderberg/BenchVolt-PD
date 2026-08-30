#![no_main]
#![no_std]

//! One-time v1→v2 migrator. Packaged as an ordinary application image at
//! 0x08008000, flashed and launched by the STOCK bootloader over its legacy
//! protocol — no SWD required. Installs the v2 boot chain in the order the
//! design doc mandates: metadata erase, then page 0 (the single
//! unrecoverable instant), then the slots. Every pass skips regions that
//! already match, so an interrupted migration resumes by simply running
//! again (the trampoline's legacy fallback re-enters this image while the
//! slots are still invalid).

use boot_core::flash;
use boot_shared::{layout, metadata};
use core::ptr::read_volatile;

static TRAMPOLINE: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/payload/trampoline.bin"));
static GOLDEN: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/payload/golden.bin"));
static WORKER: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/payload/worker.bin"));

const _: () = assert!(TRAMPOLINE.len() <= layout::TRAMPOLINE_SIZE as usize);
const _: () = assert!(GOLDEN.len() <= layout::SLOT_A_SIZE as usize);
const _: () = assert!(WORKER.len() <= layout::SLOT_B_SIZE as usize);

fn region_matches(base: u32, payload: &[u8]) -> bool {
    payload
        .iter()
        .enumerate()
        .all(|(index, byte)| unsafe { read_volatile((base as usize + index) as *const u8) } == *byte)
}

/// Erase the region's pages, program the payload, verify byte-for-byte.
fn install(base: u32, region_size: u32, payload: &[u8]) -> bool {
    let pages = (region_size as usize).div_ceil(flash::PAGE_SIZE);
    if !flash::erase_pages(base as usize, pages) {
        return false;
    }
    for (index, chunk) in payload.chunks(4).enumerate() {
        let mut word = [0xFFu8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        if !flash::program_word(base as usize + index * 4, u32::from_le_bytes(word)) {
            return false;
        }
    }
    region_matches(base, payload)
}

fn system_reset() -> ! {
    unsafe {
        cortex_m::asm::dsb();
        core::ptr::write_volatile(0xE000_ED0Cusize as *mut u32, 0x05FA_0004);
    }
    loop {
        core::hint::spin_loop();
    }
}

#[cortex_m_rt::entry]
fn main() -> ! {
    // Diagnostics marker: migrator entered.
    unsafe { core::ptr::write_volatile(0x2000_0140usize as *mut u32, 0x0000_0316) };
    cortex_m::interrupt::disable();
    boot_display::banner("MIGRATING", "DO NOT UNPLUG");

    loop {
        let mut pass_ok = true;
        if !region_matches(layout::TRAMPOLINE_BASE, TRAMPOLINE) {
            // Metadata first: from here on the v2 chain must come up with
            // the erased-default decision state (golden, no request). Then
            // page 0 — erase→program→verify is the one window in the
            // device's lifetime where a power cut needs SWD.
            pass_ok &= flash::erase_page(layout::METADATA_ADDR as usize);
            pass_ok &=
                pass_ok && install(layout::TRAMPOLINE_BASE, layout::TRAMPOLINE_SIZE, TRAMPOLINE);
        }
        if pass_ok && !region_matches(layout::SLOT_A_BASE, GOLDEN) {
            pass_ok &= install(layout::SLOT_A_BASE, layout::SLOT_A_SIZE, GOLDEN);
        }
        if pass_ok && !region_matches(layout::SLOT_B_BASE, WORKER) {
            pass_ok &= install(layout::SLOT_B_BASE, layout::SLOT_B_SIZE, WORKER);
        }
        if pass_ok {
            // Select the freshly verified worker core as the everyday boot
            // path (it carries the display banner; golden stays the silent
            // recovery core). A torn program falls back to golden — safe.
            let _ = flash::program_word(
                layout::METADATA_ADDR as usize + metadata::OFF_SLOT_FLAG * 4,
                metadata::SLOT_B_MARK,
            );
            system_reset();
        }
        // Flash trouble: brief pause, retry the whole (idempotent) pass.
        for _ in 0..4_000_000 {
            core::hint::spin_loop();
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
