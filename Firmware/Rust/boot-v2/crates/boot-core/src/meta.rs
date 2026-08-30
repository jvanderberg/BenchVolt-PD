//! Boot-metadata page operations: reads, boot-record claiming, and the
//! rebuild cycle. The page is program-only between rebuilds; every write
//! here degrades toward the erased default (golden, no request) on a torn
//! word, which is the safe direction at every instant.

use boot_shared::{layout, metadata};
use core::ptr::read_volatile;

fn word(index: usize) -> u32 {
    unsafe { read_volatile((layout::METADATA_ADDR as usize + index * 4) as *const u32) }
}

pub fn layout_word() -> u32 {
    word(metadata::OFF_LAYOUT_VERSION)
}

pub fn slot_flag() -> u32 {
    word(metadata::OFF_SLOT_FLAG)
}

pub fn request_word() -> u32 {
    word(metadata::OFF_REQUEST)
}

fn page_words() -> &'static [u32] {
    unsafe {
        core::slice::from_raw_parts(layout::METADATA_ADDR as *const u32, metadata::WORDS)
    }
}

pub fn scan_records() -> metadata::RecordScan {
    metadata::scan(page_words())
}

/// Program this boot's attempt record. Record-keeping must never block a
/// launch: a full page triggers a rebuild, and any flash failure is simply
/// ignored (the next rebuild resets the area).
pub fn claim_attempt() {
    let addr = match metadata::next_attempt_addr(page_words()) {
        Some(addr) => Some(addr),
        None => {
            if rebuild(slot_flag()) {
                metadata::next_attempt_addr(page_words())
            } else {
                None
            }
        }
    };
    if let Some(addr) = addr {
        let _ = crate::flash::program_word(addr as usize, metadata::ATTEMPT_WORD);
    }
}

/// Erase the page and reprogram layout version + slot flag (preserving a
/// slot-B selection). Clears the updater request and the record area. A
/// power cut at any point leaves erased words, i.e. golden + no request.
pub fn rebuild(flag: u32) -> bool {
    if !crate::flash::erase_page(layout::METADATA_ADDR as usize) {
        return false;
    }
    let base = layout::METADATA_ADDR as usize;
    let mut ok = crate::flash::program_word(
        base + metadata::OFF_LAYOUT_VERSION * 4,
        layout::LAYOUT_VERSION,
    );
    if metadata::flag_selects_slot_b(flag) {
        ok &= crate::flash::program_word(base + metadata::OFF_SLOT_FLAG * 4, flag);
    }
    ok
}

/// Commit a slot-B core: program the slot flag so the trampoline selects B
/// from the next boot. A torn write reads as unknown → golden (retry-safe).
pub fn select_slot_b() -> bool {
    let current = slot_flag();
    if metadata::flag_selects_slot_b(current) {
        return true;
    }
    if current != metadata::ERASED {
        // Word already holds a foreign value; only a rebuild can free it.
        if !rebuild(metadata::ERASED) {
            return false;
        }
    }
    crate::flash::program_word(
        layout::METADATA_ADDR as usize + metadata::OFF_SLOT_FLAG * 4,
        metadata::SLOT_B_MARK,
    )
}
