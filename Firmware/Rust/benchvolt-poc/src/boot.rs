use crate::benchvolt_wait_for_flash_ready;
use benchvolt_poc::settings::{
    decode as decode_settings, encode as encode_settings, PersistentSettings, RecordKind,
    SettingsRecord, RECORD_SIZE,
};

pub(crate) const BOOT_METADATA_ADDR: usize = 0x0801_F800;
const SETTINGS_ADDR: usize = 0x0801_F000;
const FLASH_PAGE_SIZE: usize = 2_048;
pub(crate) const SETTINGS_SLOTS: usize = FLASH_PAGE_SIZE / RECORD_SIZE;

#[derive(Clone, Copy)]
pub(crate) struct BootSeal {
    crc: u32,
    size: u32,
}

pub(crate) fn invalidate_boot_metadata() -> (bool, Option<BootSeal>) {
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
        let crc = core::ptr::read_volatile(BOOT_METADATA_ADDR as *const u32);
        let size = core::ptr::read_volatile((BOOT_METADATA_ADDR + 4) as *const u32);
        if crc == u32::MAX {
            return (true, None);
        }
        let seal = (size >= 192 && size <= (SETTINGS_ADDR - 0x0800_8000) as u32)
            .then_some(BootSeal { crc, size });
        if !benchvolt_wait_for_flash_ready(SR) {
            return (false, seal);
        }
        if core::ptr::read_volatile(CR) & CR_LOCK != 0 {
            core::ptr::write_volatile(KEYR, 0x4567_0123);
            core::ptr::write_volatile(KEYR, 0xcdef_89ab);
        }
        core::ptr::write_volatile(SR, SR_EOP | SR_ERRORS);
        core::ptr::write_volatile(CR, core::ptr::read_volatile(CR) | CR_PER);
        core::ptr::write_volatile(AR, BOOT_METADATA_ADDR as u32);
        core::ptr::write_volatile(CR, core::ptr::read_volatile(CR) | CR_STRT);
        let ready = benchvolt_wait_for_flash_ready(SR);
        let ok = ready
            && core::ptr::read_volatile(SR) & SR_ERRORS == 0
            && core::ptr::read_volatile(BOOT_METADATA_ADDR as *const u32) == u32::MAX;
        core::ptr::write_volatile(CR, (core::ptr::read_volatile(CR) & !CR_PER) | CR_LOCK);
        (ok, seal)
    }
}

pub(crate) fn restore_boot_seal(seal: BootSeal) -> bool {
    const FLASH_BASE: usize = 0x4002_2000;
    const KEYR: *mut u32 = (FLASH_BASE + 0x04) as *mut u32;
    const SR: *mut u32 = (FLASH_BASE + 0x0c) as *mut u32;
    const CR: *mut u32 = (FLASH_BASE + 0x10) as *mut u32;
    const SR_ERRORS: u32 = (1 << 2) | (1 << 4);
    const SR_EOP: u32 = 1 << 5;
    const CR_PG: u32 = 1 << 0;
    const CR_LOCK: u32 = 1 << 7;
    if unsafe { core::ptr::read_volatile(BOOT_METADATA_ADDR as *const u32) } != u32::MAX {
        return false;
    }
    // Size is data; CRC at offset zero is the commit marker the bootloader
    // checks. Program CRC last so a torn seal remains invalid.
    let words = [
        (BOOT_METADATA_ADDR + 4, seal.size.to_le_bytes()),
        (BOOT_METADATA_ADDR, seal.crc.to_le_bytes()),
    ];
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
        for (address, source) in words {
            for half in 0..2 {
                let offset = half * 2;
                let value = u16::from_le_bytes([source[offset], source[offset + 1]]);
                core::ptr::write_volatile((address + offset) as *mut u16, value);
                if !benchvolt_wait_for_flash_ready(SR)
                    || core::ptr::read_volatile(SR) & SR_ERRORS != 0
                {
                    core::ptr::write_volatile(
                        CR,
                        (core::ptr::read_volatile(CR) & !CR_PG) | CR_LOCK,
                    );
                    return false;
                }
            }
        }
        core::ptr::write_volatile(CR, (core::ptr::read_volatile(CR) & !CR_PG) | CR_LOCK);
        core::ptr::read_volatile(BOOT_METADATA_ADDR as *const u32) == seal.crc
            && core::ptr::read_volatile((BOOT_METADATA_ADDR + 4) as *const u32) == seal.size
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SettingsStore {
    pub(crate) latest: Option<SettingsRecord>,
    pub(crate) profiles: [Option<SettingsRecord>; 3],
    pub(crate) next_slot: usize,
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

pub(crate) fn compact_settings_store(store: &mut SettingsStore) -> bool {
    let latest = store.latest;
    let profiles = store.profiles;
    if !erase_flash_page(SETTINGS_ADDR) {
        return false;
    }
    store.next_slot = 0;
    if let Some(record) = latest {
        if !program_settings_slot(0, record) {
            return false;
        }
        store.next_slot = 1;
    }
    for record in profiles.into_iter().flatten() {
        if !program_settings_slot(store.next_slot, record) {
            return false;
        }
        store.next_slot += 1;
    }
    true
}

pub(crate) fn persist_settings_record(
    store: &mut SettingsStore,
    kind: RecordKind,
    settings: PersistentSettings,
    outputs_physically_off: bool,
) -> bool {
    // Same-bank programming can stall foreground protection before control
    // reaches the RAM-resident busy waiter. Never begin any flash mutation
    // while a power output is physically live.
    if !outputs_physically_off {
        return false;
    }
    if store.next_slot >= SETTINGS_SLOTS && !compact_settings_store(store) {
        return false;
    }
    let record = SettingsRecord {
        sequence: match kind {
            RecordKind::Autosave => store.latest,
            RecordKind::Profile(slot) => store.profiles[usize::from(slot)],
        }
        .map(|record| record.sequence.wrapping_add(1))
        .unwrap_or(1),
        kind,
        settings,
    };
    if !program_settings_slot(store.next_slot, record) {
        return false;
    }
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
) -> bool {
    persist_settings_record(
        store,
        RecordKind::Autosave,
        settings,
        outputs_physically_off,
    )
}
