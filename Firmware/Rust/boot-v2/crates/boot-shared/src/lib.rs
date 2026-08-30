//! Shared v2 boot architecture definitions: flash map, image descriptors,
//! boot metadata accounting, and the trampoline's pure decision table.
//!
//! Everything here is platform-independent so it is exercised by host tests
//! (`cargo test --target <host>`) before any of it runs on the device.

#![no_std]

pub const WORD: usize = 4;

/// Flash map (STM32F070RB, 64 pages of 2 KiB — see Docs/v2-bootloader-design.md).
pub mod layout {
    pub const TRAMPOLINE_BASE: u32 = 0x0800_0000;
    pub const TRAMPOLINE_SIZE: u32 = 0x800;
    pub const SLOT_A_BASE: u32 = 0x0800_0800;
    pub const SLOT_A_SIZE: u32 = 0x1800;
    pub const SLOT_B_BASE: u32 = 0x0800_2000;
    pub const SLOT_B_SIZE: u32 = 0x3000;
    pub const APP_BASE: u32 = 0x0800_5000;
    /// 104 KiB minus the 64-byte in-partition app descriptor.
    pub const APP_MAX_SIZE: u32 = 0x1A000 - DESC_SIZE;
    pub const DESC_ADDR: u32 = 0x0801_EFC0;
    pub const DESC_SIZE: u32 = 64;
    pub const SETTINGS_ADDR: u32 = 0x0801_F000;
    pub const SETTINGS_SIZE: u32 = 0x800;
    pub const METADATA_ADDR: u32 = 0x0801_F800;
    pub const METADATA_SIZE: u32 = 0x800;
    /// Where the v1 stock bootloader (and therefore the migrator image) lives.
    pub const LEGACY_APP_BASE: u32 = 0x0800_8000;
    pub const LEGACY_APP_MAX_SIZE: u32 = 0x17000;

    /// Vector table copied to SRAM by the v1 bootloader (contract line 12);
    /// cores reproduce the same handover for the application.
    pub const VECTOR_TABLE_SIZE: u32 = 192;
    pub const RAM_BASE: u32 = 0x2000_0000;
    pub const RAM_TOP: u32 = 0x2000_4000;

    pub const LAYOUT_VERSION: u32 = 0x3232_5642; // "BV22"
    pub const DESC_MAGIC: u32 = 0x4256_3244; // "DV2B"
    pub const SLOT_MAGIC: u32 = 0x4256_3253; // "SV2B"
}

/// Section identifiers used by the v2 upload protocol. Wire framing is the
/// v1 ACK/DATA/CRC scheme; START gains one trailing section byte.
pub mod section {
    pub const SEC_APP: u8 = 0;
    pub const SEC_SLOT_B: u8 = 1;
}

pub mod crc {
    /// Identical to flash_firmware.py::stm32_crc32 and the stock bootloader's
    /// calculate_crc32 (byte XORed into the low bits, MSB-first shifts).
    pub fn crc32(data: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFF_FFFF;
        for byte in data {
            crc ^= *byte as u32;
            for _ in 0..8 {
                if crc & 0x8000_0000 != 0 {
                    crc = (crc << 1) ^ 0x04C1_1DB7;
                } else {
                    crc <<= 1;
                }
            }
        }
        crc
    }
}

pub mod image {
    use crate::layout;

    /// A 64-byte descriptor, the fixed tail of every application image.
    /// `size` counts the payload only (everything before the descriptor).
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct AppDescriptor {
        pub layout_version: u32,
        pub size: u32,
        pub crc: u32,
    }

    /// A 16-byte descriptor occupying the last 16 bytes of a boot-core slot.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct SlotDescriptor {
        pub layout_version: u32,
        pub size: u32,
        pub crc: u32,
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct VectorCheck {
        pub initial_sp: u32,
        pub reset_vector: u32,
    }

    /// Same sanity checks the stock bootloader applies to an application
    /// (main.c ApplicationVectorsValid): SP in SRAM, Thumb reset vector
    /// inside the image bounds.
    pub fn vectors_valid(check: VectorCheck, base: u32, size: u32) -> bool {
        if check.initial_sp < layout::RAM_BASE || check.initial_sp > layout::RAM_TOP {
            return false;
        }
        if check.reset_vector & 1 == 0 {
            return false;
        }
        let entry = check.reset_vector & !1;
        entry >= base && entry < base + size
    }

    pub fn parse_app_descriptor(bytes: &[u8; 64]) -> Option<AppDescriptor> {
        let magic = u32::from_le_bytes(bytes[0..4].try_into().ok()?);
        if magic != layout::DESC_MAGIC {
            return None;
        }
        let layout_version = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
        if layout_version != layout::LAYOUT_VERSION {
            return None;
        }
        let size = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
        let crc = u32::from_le_bytes(bytes[12..16].try_into().ok()?);
        if size == 0 || size > layout::APP_MAX_SIZE {
            return None;
        }
        Some(AppDescriptor { layout_version, size, crc })
    }

    pub fn build_app_descriptor(size: u32, crc: u32) -> [u8; 64] {
        let mut bytes = [0xFF; 64];
        bytes[0..4].copy_from_slice(&layout::DESC_MAGIC.to_le_bytes());
        bytes[4..8].copy_from_slice(&layout::LAYOUT_VERSION.to_le_bytes());
        bytes[8..12].copy_from_slice(&size.to_le_bytes());
        bytes[12..16].copy_from_slice(&crc.to_le_bytes());
        bytes
    }

    pub fn parse_slot_descriptor(bytes: &[u8; 16]) -> Option<SlotDescriptor> {
        let magic = u32::from_le_bytes(bytes[0..4].try_into().ok()?);
        if magic != layout::SLOT_MAGIC {
            return None;
        }
        let layout_version = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
        if layout_version != layout::LAYOUT_VERSION {
            return None;
        }
        let size = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
        let crc = u32::from_le_bytes(bytes[12..16].try_into().ok()?);
        if size == 0 {
            return None;
        }
        Some(SlotDescriptor { layout_version, size, crc })
    }

    pub fn build_slot_descriptor(size: u32, crc: u32) -> [u8; 16] {
        let mut bytes = [0xFF; 16];
        bytes[0..4].copy_from_slice(&layout::SLOT_MAGIC.to_le_bytes());
        bytes[4..8].copy_from_slice(&layout::LAYOUT_VERSION.to_le_bytes());
        bytes[8..12].copy_from_slice(&size.to_le_bytes());
        bytes[12..16].copy_from_slice(&crc.to_le_bytes());
        bytes
    }
}

/// Boot metadata page accounting. The page is program-only between erases:
/// every word is written exactly once, so a torn or partial write can only
/// degrade towards the erased default, which selects the golden slot.
pub mod metadata {
    use crate::layout;

    /// Metadata word 0: layout version (erased = unknown → golden).
    pub const OFF_LAYOUT_VERSION: usize = 0;
    /// Metadata word 1: slot flag (erased = golden; SLOT_B_MARK = slot B).
    pub const OFF_SLOT_FLAG: usize = 1;
    /// Metadata word 2: updater request. The application programs this word
    /// for JUMP:BOOTLOADER (program-only, so the request path is crash-safe;
    /// it replaces the v1 erase-the-metadata semantics, which in v2 would no
    /// longer invalidate anything — the app CRC lives in the app partition).
    /// Any non-erased value counts: a torn program still requests updater,
    /// the safe direction. Cleared by the metadata rebuild a successful app
    /// upload performs.
    pub const OFF_REQUEST: usize = 2;
    /// Metadata word 3 onward: two-word boot records, appended sequentially.
    pub const OFF_RECORDS: usize = 3;
    /// Total 32-bit words in the metadata page.
    pub const WORDS: usize = (layout::METADATA_SIZE as usize) / 4;

    pub const SLOT_B_MARK: u32 = 0x0000_0001;
    pub const ERASED: u32 = 0xFFFF_FFFF;
    /// Canonical value the app programs into OFF_REQUEST ("UP").
    pub const REQUEST_MARK: u32 = 0x0000_5055;

    /// Any non-erased request word (including a torn program) requests
    /// updater mode.
    pub fn updater_requested(word: u32) -> bool {
        word != ERASED
    }
    /// Attempt word: bit 31 cleared, exactly once per boot attempt.
    pub const ATTEMPT_WORD: u32 = 0x7FFF_FFFF;
    /// Health word: bit 30 cleared, programmed by the app when its main loop
    /// is proven healthy.
    pub const HEALTH_WORD: u32 = 0xBFFF_FFFF;

    pub fn flag_selects_slot_b(flag: u32) -> bool {
        flag == SLOT_B_MARK
    }

    #[derive(PartialEq, Eq, Debug)]
    pub enum RecordScan {
        /// Healthy pairs followed by `unhealthy` trailing unhealthy pairs.
        Valid { healthy: usize, unhealthy: usize },
        /// No attempt word present (fresh page after erase/rebuild).
        Empty,
    }

    /// Walks the record area as (attempt, health) pairs. The first erased
    /// or malformed word ends the scan.
    pub fn scan(words: &[u32]) -> RecordScan {
        let mut healthy = 0;
        let mut unhealthy = 0;
        let mut index = OFF_RECORDS;
        while index + 1 < words.len() {
            let attempt = words[index];
            if attempt == ERASED {
                break;
            }
            if attempt & 0x8000_0000 != 0 {
                // Malformed attempt word: treat as end of records.
                break;
            }
            if words[index + 1] == HEALTH_WORD {
                healthy += 1;
            } else {
                unhealthy += 1;
            }
            index += 2;
        }
        if healthy + unhealthy == 0 {
            RecordScan::Empty
        } else {
            RecordScan::Valid { healthy, unhealthy }
        }
    }

    /// True when no full (attempt, health) pair slot remains.
    pub fn records_full(words: &[u32]) -> bool {
        next_attempt_index(words).is_none()
    }

    /// Index of the next PAIR-ALIGNED attempt slot: records stride by two
    /// words, and the attempt must never land in the previous record's
    /// health slot (an earlier word-stride scan did exactly that, which
    /// broke both the unhealthy count and the app's find-my-health-word
    /// convention).
    fn next_attempt_index(words: &[u32]) -> Option<usize> {
        let mut index = OFF_RECORDS;
        while index + 1 < words.len() {
            if words[index] == ERASED {
                return Some(index);
            }
            index += 2;
        }
        None
    }

    /// Address of the next attempt word to program, if any.
    pub fn next_attempt_addr(words: &[u32]) -> Option<u32> {
        next_attempt_index(words).map(|index| layout::METADATA_ADDR + (index * 4) as u32)
    }

    /// Address of the health word that follows the attempt word at `addr`.
    pub fn health_addr_for(attempt_addr: u32) -> u32 {
        attempt_addr + 4
    }
}

/// The trampoline's pure decision table. `a_valid`/`b_valid` come from the
/// slot vector sanity checks; `legacy_valid` from the same checks applied to
/// the v1 application entry at `LEGACY_APP_BASE`.
#[derive(PartialEq, Eq, Debug)]
pub enum BootTarget {
    Golden,
    SlotB,
    Legacy,
    Halt,
}

pub fn trampoline_decision(
    slot_flag: u32,
    encoder_pressed: bool,
    a_valid: bool,
    b_valid: bool,
    legacy_valid: bool,
) -> BootTarget {
    if encoder_pressed {
        // Physical interlock: reach an updater — golden first, then the
        // working core's updater, then the migration-window legacy entry.
        if a_valid {
            BootTarget::Golden
        } else if b_valid {
            BootTarget::SlotB
        } else if legacy_valid {
            BootTarget::Legacy
        } else {
            BootTarget::Halt
        }
    } else if metadata::flag_selects_slot_b(slot_flag) && b_valid {
        BootTarget::SlotB
    } else if a_valid {
        BootTarget::Golden
    } else if b_valid {
        BootTarget::SlotB
    } else if legacy_valid {
        BootTarget::Legacy
    } else {
        BootTarget::Halt
    }
}

/// Boot policy: the working core refuses to launch the application after N
/// consecutive unhealthy boots and stays in updater mode instead.
pub const UNHEALTHY_BOOT_LIMIT: usize = 3;

pub fn should_stay_in_updater(scan: &metadata::RecordScan) -> bool {
    match scan {
        metadata::RecordScan::Empty => false,
        metadata::RecordScan::Valid { unhealthy, .. } => *unhealthy >= UNHEALTHY_BOOT_LIMIT,
    }
}

/// What a selected core does after the trampoline hands over.
#[derive(PartialEq, Eq, Debug)]
pub enum CoreBoot {
    LaunchApp,
    Updater,
}

/// The core's pure boot decision. `layout_word` is metadata word 0: erased
/// is acceptable (fresh migration / interrupted rebuild — the app descriptor
/// alone decides validity), but a programmed value for a DIFFERENT layout
/// means the flash map disagrees with this core and only the updater is
/// safe. `app_valid` is the full descriptor + CRC + vector check.
pub fn core_boot_decision(
    layout_word: u32,
    request_word: u32,
    scan: &metadata::RecordScan,
    app_valid: bool,
) -> CoreBoot {
    if layout_word != metadata::ERASED && layout_word != layout::LAYOUT_VERSION {
        return CoreBoot::Updater;
    }
    if metadata::updater_requested(request_word) {
        return CoreBoot::Updater;
    }
    if should_stay_in_updater(scan) {
        return CoreBoot::Updater;
    }
    if app_valid {
        CoreBoot::LaunchApp
    } else {
        CoreBoot::Updater
    }
}
