//! v2 sectioned upload protocol over the polled CDC stack.
//!
//! Wire framing is v1-compatible (START/DATA/END, single-byte ACK/NACK,
//! 64-byte CDC packets) with START extended by one section byte.

use boot_shared::{image, layout, section};

pub const ACK: u8 = 0x06;
pub const NACK: u8 = 0x15;
pub const CMD_START: u8 = 0x01;
pub const CMD_DATA: u8 = 0x02;
pub const CMD_END: u8 = 0x03;
/// Reboot the device (v1's CMD_JUMP_ONLY slot): ACK, then system reset —
/// the trampoline re-decides and a freshly committed app launches.
pub const CMD_BOOT: u8 = 0x04;
pub const CMD_INFO: u8 = 0x10;

const MAX_CHUNK: usize = 60;

pub struct UploadState {
    pub active: bool,
    pub section: u8,
    base: u32,
    written: usize,
    total: usize,
}

impl UploadState {
    pub const fn new() -> Self {
        UploadState { active: false, section: 0, base: 0, written: 0, total: 0 }
    }

    pub fn reset(&mut self) {
        self.active = false;
        self.written = 0;
    }

    pub fn written(&self) -> usize {
        self.written
    }
}

/// Section geometry: (base, max image bytes). The app section excludes the
/// 64-byte descriptor region, which END programs last as the commit step.
fn section_bounds(section: u8) -> Option<(u32, usize)> {
    match section {
        section::SEC_APP => Some((layout::APP_BASE, layout::APP_MAX_SIZE as usize)),
        section::SEC_SLOT_B => Some((layout::SLOT_B_BASE, layout::SLOT_B_SIZE as usize - 16)),
        _ => None,
    }
}

/// START <size:u32 LE> <section:u8> — erase the section, stage the transfer.
pub fn cmd_start(state: &mut UploadState, body: &[u8]) -> u8 {
    if body.len() != 5 {
        return NACK;
    }
    let total = u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
    let section = body[4];
    let Some((base, capacity)) = section_bounds(section) else {
        return NACK;
    };
    if total < 192 || total > capacity {
        return NACK;
    }
    // Erase the FULL section (v1 semantics), not just the pages the image
    // covers: the app descriptor (and a slot's descriptor tail) lives in the
    // section's last page, and a survivor from the previous image would make
    // the END commit's program-into-programmed-words fail — or worse, leave
    // stale trailing content under a valid-looking descriptor.
    let section_bytes = match section {
        section::SEC_APP => (layout::DESC_ADDR + layout::DESC_SIZE - layout::APP_BASE) as usize,
        _ => layout::SLOT_B_SIZE as usize,
    };
    let pages = (section_bytes + 2047) / 2048;
    if !crate::flash::erase_pages(base as usize, pages) {
        return NACK;
    }
    *state = UploadState { active: true, section, base, written: 0, total };
    ACK
}

/// DATA <len:u16 LE> <bytes> — sequential chunks only, word-aligned except
/// the final chunk (matches v1 semantics).
pub fn cmd_data(state: &mut UploadState, body: &[u8]) -> u8 {
    if !state.active || body.len() < 2 {
        diag(3, state.active as u32, body.len() as u32);
        return NACK;
    }
    let len = u16::from_le_bytes([body[0], body[1]]) as usize;
    if len == 0 || len > MAX_CHUNK || body.len() != len + 2 {
        diag(4, len as u32, body.len() as u32);
        // Dump the packet head and live EP1/PMA state for the count bug.
        unsafe {
            for word in 0..3 {
                let mut bytes = [0u8; 4];
                for (i, b) in bytes.iter_mut().enumerate() {
                    *b = *body.get(word * 4 + i).unwrap_or(&0xEE);
                }
                core::ptr::write_volatile(
                    (0x2000_0154usize + word * 4) as *mut u32,
                    u32::from_le_bytes(bytes),
                );
            }
            core::ptr::write_volatile(
                0x2000_0160usize as *mut u32,
                (boot_usb::regs::pma_count_rx(1) as u32) << 16
                    | boot_usb::regs::ep_read(1) as u32,
            );
        }
        return NACK;
    }
    let payload = &body[2..2 + len];
    if state.written + len > state.total || (state.written + len < state.total && len % 4 != 0) {
        diag(5, state.written as u32, len as u32);
        state.reset();
        return NACK;
    }
    for (index, chunk) in payload.chunks(4).enumerate() {
        let mut word = [0xFFu8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        let value = u32::from_le_bytes(word);
        let address = state.base as usize + state.written + index * 4;
        if !crate::flash::program_word(address, value) {
            state.reset();
            return NACK;
        }
    }
    state.written += len;
    ACK
}

/// END <crc:u32 LE> — verify the section CRC; for the app section, program
/// the descriptor last as the commit step. ACK only on full success.
pub fn cmd_end(state: &mut UploadState, body: &[u8]) -> u8 {
    if !state.active || body.len() != 4 || state.written != state.total {
        diag(6, state.written as u32, ((state.active as u32) << 16) | body.len() as u32);
        state.reset();
        return NACK;
    }
    let expected = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
    let bytes = unsafe { core::slice::from_raw_parts(state.base as *const u8, state.written) };
    let computed = boot_shared::crc::crc32(bytes);
    if computed != expected {
        // Bring-up diagnostics: CRC-stage NACK (site 2).
        unsafe {
            core::ptr::write_volatile(0x2000_0150usize as *mut u32, 0x0002_0000);
            core::ptr::write_volatile(0x2000_0148usize as *mut u32, computed);
            core::ptr::write_volatile(0x2000_014Cusize as *mut u32, expected);
        }
        state.reset();
        return NACK;
    }
    if state.section == section::SEC_APP {
        let descriptor = image::build_app_descriptor(state.written as u32, expected);
        if !program_descriptor(layout::DESC_ADDR as usize, &descriptor) {
            state.reset();
            return NACK;
        }
        // Fresh image: reset the boot records and clear any updater request
        // (this is what ends a JUMP:BOOTLOADER session). Slot selection is
        // preserved. Failure here must not fail the upload — the image and
        // its descriptor are already committed, and stale metadata only
        // costs an extra updater boot.
        let _ = crate::meta::rebuild(crate::meta::slot_flag());
    }
    if state.section == section::SEC_SLOT_B {
        // Commit order per the design doc: descriptor into the slot's last
        // 16 bytes, then the flag flip — a single word program. A power cut
        // between the two leaves the flag pointing at the old core.
        let descriptor = image::build_slot_descriptor(state.written as u32, expected);
        let tail = (layout::SLOT_B_BASE + layout::SLOT_B_SIZE) as usize - 16;
        if !program_descriptor(tail, &descriptor) || !crate::meta::select_slot_b() {
            state.reset();
            return NACK;
        }
    }
    state.reset();
    ACK
}

fn program_descriptor(address: usize, bytes: &[u8]) -> bool {
    for (index, chunk) in bytes.chunks(4).enumerate() {
        let mut word = [0xFFu8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        let value = u32::from_le_bytes(word);
        if value != 0xFFFF_FFFF && !crate::flash::program_word(address + index * 4, value) {
            return false;
        }
    }
    true
}

/// INFO — reply with identity: b"BV2C" + layout version + section caps.
pub fn cmd_info() -> [u8; 12] {
    let mut out = [0u8; 12];
    out[0..4].copy_from_slice(b"BV2C");
    out[4..8].copy_from_slice(&layout::LAYOUT_VERSION.to_le_bytes());
    out[8..12].copy_from_slice(&layout::APP_MAX_SIZE.to_le_bytes());
    out
}

/// Bring-up diagnostics: NACK site + two values (0x20000148..0x20000150).
fn diag(site: u32, a: u32, b: u32) {
    unsafe {
        core::ptr::write_volatile(0x2000_0148usize as *mut u32, a);
        core::ptr::write_volatile(0x2000_014Cusize as *mut u32, b);
        core::ptr::write_volatile(0x2000_0150usize as *mut u32, site << 16);
    }
}

/// Dispatch one command packet (body excludes the command byte).
/// `allow_core` gates the slot-B section behind the physical interlock.
pub fn dispatch(state: &mut UploadState, cmd: u8, body: &[u8], allow_core: bool) -> Option<u8> {
    match cmd {
        CMD_START => {
            if !allow_core && body.get(4) != Some(&section::SEC_APP) {
                Some(NACK)
            } else {
                Some(cmd_start(state, body))
            }
        }
        CMD_DATA => Some(cmd_data(state, body)),
        CMD_END => Some(cmd_end(state, body)),
        CMD_INFO => Some(ACK),
        _ => Some(NACK),
    }
}

