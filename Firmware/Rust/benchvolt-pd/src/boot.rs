use crate::benchvolt_wait_for_flash_ready;
use benchvolt_pd::settings::{
    decode as decode_settings, encode as encode_settings, PersistentSettings, RecordKind,
    SettingsRecord, RECORD_SIZE, SEQUENCE_MASK,
};

pub(crate) const BOOT_METADATA_ADDR: usize = 0x0801_F800;
const SETTINGS_ADDR: usize = 0x0801_F000;

/// v2 boot support: the metadata page is program-only from the app's side.
/// JUMP:BOOTLOADER programs the request word; the healthy main loop
/// programs the health word of the boot record the core claimed at launch.
/// Both are single-word programs (~100 µs stall) — bounded, unlike the
/// erase the v1 path used.
#[cfg(feature = "v2-boot")]
mod v2 {
    use crate::benchvolt_wait_for_flash_ready;
    use boot_shared::metadata;

    const FLASH_BASE: usize = 0x4002_2000;
    const KEYR: *mut u32 = (FLASH_BASE + 0x04) as *mut u32;
    const SR: *mut u32 = (FLASH_BASE + 0x0c) as *mut u32;
    const CR: *mut u32 = (FLASH_BASE + 0x10) as *mut u32;
    const SR_ERRORS: u32 = (1 << 2) | (1 << 4);
    const SR_EOP: u32 = 1 << 5;
    const CR_PG: u32 = 1 << 0;
    const CR_LOCK: u32 = 1 << 7;

    fn program_word(address: usize, value: u32) -> bool {
        unsafe {
            if !benchvolt_wait_for_flash_ready(SR) {
                return false;
            }
            if core::ptr::read_volatile(CR) & CR_LOCK != 0 {
                core::ptr::write_volatile(KEYR, 0x4567_0123);
                core::ptr::write_volatile(KEYR, 0xcdef_89ab);
            }
            core::ptr::write_volatile(SR, SR_EOP | SR_ERRORS);
            core::ptr::write_volatile(CR, core::ptr::read_volatile(CR) | CR_PG);
            core::ptr::write_volatile(address as *mut u16, (value & 0xFFFF) as u16);
            let first = benchvolt_wait_for_flash_ready(SR);
            if first {
                core::ptr::write_volatile((address + 2) as *mut u16, (value >> 16) as u16);
            }
            let ok = first
                && benchvolt_wait_for_flash_ready(SR)
                && core::ptr::read_volatile(SR) & SR_ERRORS == 0;
            core::ptr::write_volatile(CR, (core::ptr::read_volatile(CR) & !CR_PG) | CR_LOCK);
            ok && core::ptr::read_volatile(address as *const u32) == value
        }
    }

    fn metadata_word(index: usize) -> u32 {
        unsafe {
            core::ptr::read_volatile((super::BOOT_METADATA_ADDR + index * 4) as *const u32)
        }
    }

    /// Request updater mode for the next boot. Idempotent: an already
    /// non-erased request word (including a torn one) already requests it.
    pub(crate) fn request_bootloader() -> bool {
        if metadata::updater_requested(metadata_word(metadata::OFF_REQUEST)) {
            return true;
        }
        program_word(
            super::BOOT_METADATA_ADDR + metadata::OFF_REQUEST * 4,
            metadata::REQUEST_MARK,
        )
    }

    /// Mark this boot's record healthy: the launching core programmed the
    /// LAST attempt word (pair-strided); its health word follows it.
    pub(crate) fn mark_boot_healthy() -> bool {
        let mut last_attempt: Option<usize> = None;
        let mut index = metadata::OFF_RECORDS;
        while index + 1 < metadata::WORDS {
            if metadata_word(index) == metadata::ERASED {
                break;
            }
            last_attempt = Some(index);
            index += 2;
        }
        let Some(attempt) = last_attempt else {
            // No record claimed (e.g. fresh metadata layouts) — nothing to do.
            return true;
        };
        if metadata_word(attempt + 1) == metadata::HEALTH_WORD {
            return true;
        }
        program_word(
            super::BOOT_METADATA_ADDR + (attempt + 1) * 4,
            metadata::HEALTH_WORD,
        )
    }
}

#[cfg(feature = "v2-boot")]
pub(crate) use v2::{mark_boot_healthy, request_bootloader};
const FLASH_PAGE_SIZE: usize = 2_048;
pub(crate) const SETTINGS_SLOTS: usize = FLASH_PAGE_SIZE / RECORD_SIZE;

#[derive(Clone, Copy)]
pub(crate) struct SettingsStore {
    pub(crate) latest: Option<SettingsRecord>,
    pub(crate) profiles: [Option<SettingsRecord>; 3],
    pub(crate) next_slot: usize,
    /// Consecutive record-program failures this session. A small budget of
    /// dirty-slot skips recovers from a transient glitch; past it, persistence
    /// gives up for the session (data in flash stays intact) instead of
    /// marching slot-by-slot into a destructive compaction of a page that
    /// cannot be reprogrammed.
    pub(crate) program_failures: u8,
}

fn read_settings_slot(slot: usize) -> [u8; RECORD_SIZE] {
    let mut bytes = [0; RECORD_SIZE];
    for (offset, byte) in bytes.iter_mut().enumerate() {
        *byte = unsafe {
            core::ptr::read_volatile((SETTINGS_ADDR + slot * RECORD_SIZE + offset) as *const u8)
        };
    }
    bytes
}

pub(crate) fn load_settings_store() -> SettingsStore {
    let mut latest: Option<SettingsRecord> = None;
    let mut profiles: [Option<SettingsRecord>; 3] = [None; 3];
    let mut next_slot = SETTINGS_SLOTS;
    for slot in 0..SETTINGS_SLOTS {
        let bytes = read_settings_slot(slot);
        if bytes.iter().all(|byte| *byte == 0xff) {
            next_slot = next_slot.min(slot);
        } else if let Some(record) = decode_settings(&bytes) {
            let destination = match record.kind {
                RecordKind::Autosave => &mut latest,
                RecordKind::Profile(slot) => &mut profiles[usize::from(slot)],
            };
            if destination
                .map(|old| record.sequence > old.sequence)
                .unwrap_or(true)
            {
                *destination = Some(record);
            }
        }
    }
    SettingsStore {
        latest,
        profiles,
        next_slot,
        program_failures: 0,
    }
}

pub(crate) fn erase_flash_page(address: usize) -> bool {
    const FLASH_BASE: usize = 0x4002_2000;
    const KEYR: *mut u32 = (FLASH_BASE + 0x04) as *mut u32;
    const SR: *mut u32 = (FLASH_BASE + 0x0c) as *mut u32;
    const CR: *mut u32 = (FLASH_BASE + 0x10) as *mut u32;
    const AR: *mut u32 = (FLASH_BASE + 0x14) as *mut u32;
    const SR_ERRORS: u32 = (1 << 2) | (1 << 4);
    const SR_EOP: u32 = 1 << 5;
    const CR_PER: u32 = 1 << 1;
    const CR_STRT: u32 = 1 << 6;
    const CR_LOCK: u32 = 1 << 7;
    unsafe {
        if !benchvolt_wait_for_flash_ready(SR) {
            return false;
        }
        if core::ptr::read_volatile(CR) & CR_LOCK != 0 {
            core::ptr::write_volatile(KEYR, 0x4567_0123);
            core::ptr::write_volatile(KEYR, 0xcdef_89ab);
        }
        core::ptr::write_volatile(SR, SR_EOP | SR_ERRORS);
        core::ptr::write_volatile(CR, core::ptr::read_volatile(CR) | CR_PER);
        core::ptr::write_volatile(AR, address as u32);
        core::ptr::write_volatile(CR, core::ptr::read_volatile(CR) | CR_STRT);
        let ready = benchvolt_wait_for_flash_ready(SR);
        let ok = ready
            && core::ptr::read_volatile(SR) & SR_ERRORS == 0
            && (0..FLASH_PAGE_SIZE)
                .all(|offset| core::ptr::read_volatile((address + offset) as *const u8) == 0xff);
        core::ptr::write_volatile(CR, (core::ptr::read_volatile(CR) & !CR_PER) | CR_LOCK);
        ok
    }
}

fn program_settings_slot(slot: usize, record: SettingsRecord) -> bool {
    if slot >= SETTINGS_SLOTS {
        return false;
    }
    let address = SETTINGS_ADDR + slot * RECORD_SIZE;
    if !(0..RECORD_SIZE)
        .all(|offset| unsafe { core::ptr::read_volatile((address + offset) as *const u8) == 0xff })
    {
        return false;
    }
    const FLASH_BASE: usize = 0x4002_2000;
    const KEYR: *mut u32 = (FLASH_BASE + 0x04) as *mut u32;
    const SR: *mut u32 = (FLASH_BASE + 0x0c) as *mut u32;
    const CR: *mut u32 = (FLASH_BASE + 0x10) as *mut u32;
    const SR_ERRORS: u32 = (1 << 2) | (1 << 4);
    const SR_EOP: u32 = 1 << 5;
    const CR_PG: u32 = 1 << 0;
    const CR_LOCK: u32 = 1 << 7;
    let bytes = encode_settings(record);
    let mut ok = true;
    unsafe {
        if !benchvolt_wait_for_flash_ready(SR) {
            return false;
        }
        if core::ptr::read_volatile(CR) & CR_LOCK != 0 {
            core::ptr::write_volatile(KEYR, 0x4567_0123);
            core::ptr::write_volatile(KEYR, 0xcdef_89ab);
        }
        core::ptr::write_volatile(SR, SR_EOP | SR_ERRORS);
        core::ptr::write_volatile(CR, core::ptr::read_volatile(CR) | CR_PG);
        for offset in (0..RECORD_SIZE).step_by(2) {
            let value = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
            core::ptr::write_volatile((address + offset) as *mut u16, value);
            if !benchvolt_wait_for_flash_ready(SR) || core::ptr::read_volatile(SR) & SR_ERRORS != 0
            {
                ok = false;
                break;
            }
        }
        core::ptr::write_volatile(CR, (core::ptr::read_volatile(CR) & !CR_PG) | CR_LOCK);
    }
    ok && decode_settings(&read_settings_slot(slot)) == Some(record)
}

/// Compaction erases the ONLY settings page before rewriting the live
/// records from RAM, so it refuses to start once the program-failure budget
/// is spent (a flash that rejects programs must keep its existing records),
/// and each rewrite failure is charged against the budget with a bounded
/// skip-and-retry so one marginal slot does not abandon the remaining
/// records. Returns false when any record failed to land; RAM keeps every
/// record either way, so the loss surfaces only at the next boot.
pub(crate) fn compact_settings_store(store: &mut SettingsStore) -> bool {
    if store.program_failures >= PROGRAM_FAILURE_BUDGET {
        return false;
    }
    let latest = store.latest;
    let profiles = store.profiles;
    if !erase_flash_page(SETTINGS_ADDR) {
        return false;
    }
    store.next_slot = 0;
    let mut all_programmed = true;
    for record in core::iter::once(latest).chain(profiles).flatten() {
        let mut programmed = false;
        while store.next_slot < SETTINGS_SLOTS
            && store.program_failures < PROGRAM_FAILURE_BUDGET
        {
            if program_settings_slot(store.next_slot, record) {
                store.program_failures = 0;
                store.next_slot += 1;
                programmed = true;
                break;
            }
            store.program_failures += 1;
            store.next_slot += 1;
        }
        all_programmed &= programmed;
    }
    all_programmed
}

const PROGRAM_FAILURE_BUDGET: u8 = 3;

pub(crate) fn persist_settings_record(
    store: &mut SettingsStore,
    kind: RecordKind,
    settings: PersistentSettings,
    outputs_physically_off: bool,
    allow_compaction: bool,
) -> bool {
    // Same-bank programming can stall foreground protection before control
    // reaches the RAM-resident busy waiter. Never begin any flash mutation
    // while a power output is physically live.
    if !outputs_physically_off {
        return false;
    }
    // Past the failure budget the flash is not accepting programs; stop
    // burning slots (and never compact) so the existing records survive.
    if store.program_failures >= PROGRAM_FAILURE_BUDGET {
        return false;
    }
    if store.next_slot >= SETTINGS_SLOTS {
        // Compaction is admitted only in the caller's quiet window (loop
        // healthy, outside the boot attach churn). compact_settings_store
        // itself enforces the failure budget, so a transient failure that
        // happened to land on the final slot still gets its remaining
        // retries instead of deadlocking persistence for the session.
        if !allow_compaction || !compact_settings_store(store) {
            return false;
        }
    }
    let record = SettingsRecord {
        sequence: match kind {
            RecordKind::Autosave => store.latest,
            RecordKind::Profile(slot) => store.profiles[usize::from(slot)],
        }
        .map(|record| record.sequence.wrapping_add(1) & SEQUENCE_MASK)
        .unwrap_or(1),
        kind,
        settings,
    };
    if !program_settings_slot(store.next_slot, record) {
        // The failed slot may no longer be blank, and the blank-check would
        // then refuse it forever. Skip past it (within the failure budget)
        // so the next attempt uses a fresh slot instead of wedging every
        // persistence path; the dirty slot decodes as garbage and is
        // ignored at the next load.
        store.program_failures += 1;
        store.next_slot += 1;
        return false;
    }
    store.program_failures = 0;
    match kind {
        RecordKind::Autosave => store.latest = Some(record),
        RecordKind::Profile(slot) => store.profiles[usize::from(slot)] = Some(record),
    }
    store.next_slot += 1;
    true
}

pub(crate) fn persist_settings(
    store: &mut SettingsStore,
    settings: PersistentSettings,
    outputs_physically_off: bool,
    allow_compaction: bool,
) -> bool {
    persist_settings_record(
        store,
        RecordKind::Autosave,
        settings,
        outputs_physically_off,
        allow_compaction,
    )
}
