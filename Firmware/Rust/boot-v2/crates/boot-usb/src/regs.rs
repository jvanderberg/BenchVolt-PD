//! Verified register layer for the STM32F070 USB peripheral (RM0004 §17.4).
//!
//! EPnR bit positions from `stm32f070xb.h:5357-5394`; the modify idiom
//! mirrors `stm32-usbd`'s proven `set_invariant_values` (this silicon works
//! with the app on it): CTR flags written 1 (rc_w0: preserved), DTOGs
//! written 0 (preserve), STAT fields written as XOR of target vs current.

// (module of the no_std boot-usb crate)

use core::ptr::{read_volatile, write_volatile};


pub const CNTR: *mut u32 = 0x4000_5C40usize as *mut u32;
pub const ISTR: *mut u32 = 0x4000_5C44usize as *mut u32;
pub const DADDR: *mut u32 = 0x4000_5C4Cusize as *mut u32;
pub const BTABLE: *mut u32 = 0x4000_5C50usize as *mut u32;
pub const BCDR: *mut u32 = 0x4000_5C58usize as *mut u32;
pub const BCDR_DPPU: u32 = 1 << 15;

pub const CNTR_FRES: u32 = 1 << 0;
pub const CNTR_PDWN: u32 = 1 << 1;
pub const CNTR_CTRM: u32 = 1 << 15;
pub const CNTR_RESETM: u32 = 1 << 10;
pub const DADDR_EF: u32 = 1 << 7;
pub const ISTR_RESET: u32 = 1 << 10;

// EPnR bits (stm32f070xb.h:5357-5394).
pub const EP_CTR_RX: u16 = 0x8000;
pub const EP_DTOG_RX: u16 = 0x4000;
pub const EP_STAT_RX: u16 = 0x3000;
pub const EP_SETUP: u16 = 0x0800;
pub const EP_TYPE_CONTROL: u16 = 0x0200;
pub const EP_TYPE_BULK: u16 = 0x0000;
pub const EP_TYPE_INTERRUPT: u16 = 0x0600;
pub const EP_KIND: u16 = 0x0100;
pub const EP_CTR_TX: u16 = 0x0080;
pub const EP_DTOG_TX: u16 = 0x0040;
pub const EP_STAT_TX: u16 = 0x0030;
pub const EP_ADDR_FIELD: u16 = 0x000F;
pub const EP_T_FIELD: u16 = 0x0600;

// Status field encodings (2-bit field values).
pub const STATUS_DISABLED: u8 = 0b00;
pub const STAT_DISABLED: u8 = 0b00;
pub const STAT_STALL: u8 = 0b01;
pub const STAT_NAK: u8 = 0b10;
pub const STAT_VALID: u8 = 0b11;

pub const USB_BASE: usize = 0x4000_5C00;
pub const PMA: usize = 0x4000_6000;

const fn ep(n: usize) -> *mut u16 {
    (USB_BASE + 4 * n) as *mut u16
}

pub fn ep_read(n: usize) -> u16 {
    unsafe { read_volatile(ep(n)) }
}

/// Write EPnR with stm32-usbd semantics: `stat_tx`/`stat_rx` are the 2-bit
/// fields to XOR into the current values (None = preserve, i.e. write 0);
/// `clear_ctr_rx/tx` write 0 into the CTR flags to clear them; type/kind/
/// address are written back as read.
pub fn ep_modify(n: usize, stat_tx: Option<u8>, stat_rx: Option<u8>, clear_ctr_rx: bool, clear_ctr_tx: bool) {
    let r = unsafe { read_volatile(ep(n)) };
    // Type + KIND + EA: R/W fields, written back as read.
    let mut w = r & (EP_T_FIELD | EP_KIND | EP_ADDR_FIELD);
    // Toggle fields: invariant 0 unless XOR bits requested.
    let cur_tx = ((r & EP_STAT_TX) >> 4) as u8;
    let cur_rx = ((r & EP_STAT_RX) >> 12) as u8;
    let tx_bits = match stat_tx {
        Some(target) => (cur_tx ^ target) as u16,
        None => 0,
    };
    let rx_bits = match stat_rx {
        Some(t) => (cur_rx ^ t) as u16,
        None => 0,
    };
    w |= (tx_bits & 0b11) << 4;
    w |= ((rx_bits & 0b11) as u16) << 12;
    // CTR flags: rc_w0 — write 1 to leave alone, 0 to clear.
    if !clear_ctr_rx {
        w |= EP_CTR_RX;
    }
    if !clear_ctr_tx {
        w |= EP_CTR_TX;
    }
    unsafe {
        // 16-bit access: the USB register file is halfword-wide (the HAL
        // uses uint16_t writes exclusively for EPnR).
        write_volatile((USB_BASE + 4 * n) as *mut u16, w);
    }
}

/// Initialize an endpoint register into a known armed state from ANY
/// current state. STAT and DTOG bits are toggle-on-write-1, so the write
/// value is computed against the current register: STAT = current XOR
/// target, DTOG = current (toggles set bits back to 0). Pending CTR flags
/// are cleared (write 0). Writing a fixed value here instead — as an
/// earlier revision did — disables an endpoint that is already armed,
/// which is exactly how EP0 died mid-enumeration on hardware.
pub fn ep_init(n: usize, ep_type: u16, stat_tx: u8, stat_rx: u8) {
    let r = unsafe { read_volatile(ep(n)) };
    let cur_tx = ((r & EP_STAT_TX) >> 4) as u16;
    let cur_rx = ((r & EP_STAT_RX) >> 12) as u16;
    let w = ep_type
        | (n as u16 & EP_ADDR_FIELD)
        | (r & (EP_DTOG_RX | EP_DTOG_TX))
        | ((cur_tx ^ stat_tx as u16) << 4)
        | ((cur_rx ^ stat_rx as u16) << 12);
    unsafe {
        // 16-bit access: the USB register file is halfword-wide (the HAL
        // uses uint16_t writes exclusively for EPnR).
        write_volatile((USB_BASE + 4 * n) as *mut u16, w);
    }
}

pub fn set_stat_tx(n: usize, target: u8) {
    ep_modify(n, Some(target), None, false, false);
}

pub fn set_stat_rx(n: usize, target: u8) {
    ep_modify(n, None, Some(target), false, false);
}

pub fn clear_ctr_rx(n: usize) {
    ep_modify(n, None, None, true, false);
}

pub fn clear_ctr_tx(n: usize) {
    ep_modify(n, None, None, false, true);
}

pub fn ep_type_of(n: usize) -> u16 {
    unsafe { read_volatile(ep(n)) & EP_T_FIELD }
}

// ---- PMA ------------------------------------------------------------------

/// Buffer descriptor table OFFSET for endpoint n (8 bytes each):
/// [TX addr, TX count, RX addr, RX count]. Offsets are relative to the PMA
/// base; the accessor helpers below add PMA exactly once.
pub const fn bd(n: usize) -> usize {
    n * 8
}

pub fn pma_r(offset: usize) -> u16 {
    unsafe { read_volatile((PMA + offset) as *mut u16) }
}

pub fn pma_read_bytes(buf: u16, out: &mut [u8]) {
    // PMA bytes are packed two per 16-bit halfword (little-endian): byte 2i
    // = low byte of halfword i, byte 2i+1 = high byte.
    for (index, byte) in out.iter_mut().enumerate() {
        let word = unsafe { read_volatile((PMA + buf as usize + (index & !1)) as *mut u16) };
        *byte = if index & 1 == 0 {
            (word & 0xFF) as u8
        } else {
            (word >> 8) as u8
        };
    }
}


pub fn pma_write_bytes(buf: u16, data: &[u8]) {
    for (index, chunk) in data.chunks(2).enumerate() {
        let word = if chunk.len() == 2 {
            u16::from_le_bytes([chunk[0], chunk[1]])
        } else {
            chunk[0] as u16
        };
        pma_w(buf as usize + 2 * index, word);
    }
}

fn pma_w(offset: usize, value: u16) {
    unsafe { write_volatile((PMA + offset) as *mut u16, value) };
}

pub fn pma_half(offset: usize, value: u16) {
    unsafe { write_volatile((PMA + offset) as *mut u16, value) };
}

pub fn pma_count_tx(n: usize, bytes: u16) {
    pma_half(bd(n) + 2, bytes);
}

/// STAT_TX field of a raw EPnR value as a 2-bit field value.
pub fn stat_tx_of(reg: u16) -> u8 {
    ((reg & EP_STAT_TX) >> 4) as u8
}

pub fn spin(cycles: u32) {
    for _ in 0..cycles {
        core::hint::spin_loop();
    }
}

pub const CR_HSEON: u32 = 1 << 16;
pub const CR_HSERDY: u32 = 1 << 17;
pub const CR_PLLON: u32 = 1 << 24;
pub const CR_PLLRDY: u32 = 1 << 25;
pub const CFGR_SW_MASK: u32 = 0b11;
pub const CFGR_SW_PLL: u32 = 0b10;
pub const CFGR_SWS_MASK: u32 = 0b11 << 2;
pub const CFGR_SWS_PLL: u32 = 0b10 << 2;
pub const CFGR_PLLSRC_HSE: u32 = 1 << 16;
pub const CFGR_PLLMUL_MASK: u32 = 0b1111 << 18;
pub const CFGR_PLLMUL_X6: u32 = 0b0100 << 18;
pub const RCC_APB1ENR_USBEN: u32 = 1 << 23;

// ---- RCC constants ----

pub const RCC_CR: *mut u32 = 0x4002_1000usize as *mut u32;
pub const RCC_CFGR: *mut u32 = 0x4002_1004usize as *mut u32;
pub const RCC_CFGR3: *mut u32 = 0x4002_1030usize as *mut u32;
/// USBSW = 1: USB clock from PLL. The F070 has no HSI48, so the reset value
/// (0) leaves the USB peripheral entirely unclocked; the stock bootloader
/// sets this via HAL (RCC_USBCLKSOURCE_PLL) and a cold-booted core must too.
pub const CFGR3_USBSW_PLL: u32 = 1 << 7;
pub const RCC_APB1ENR: *mut u32 = 0x4002_101Cusize as *mut u32;
pub const RCC_APB1RSTR: *mut u32 = 0x4002_1010usize as *mut u32;
pub const RCC_APB1RSTR_USBRST: u32 = 1 << 23;


pub fn write_cntr(value: u32) {
    unsafe { write_volatile(CNTR, value) };
}
pub fn read_cntr() -> u32 {
    unsafe { read_volatile(CNTR) }
}
pub fn write_istr(value: u32) {
    unsafe { write_volatile(ISTR, value) };
}
pub fn read_istr() -> u32 {
    unsafe { read_volatile(ISTR) }
}
pub fn write_btable(value: u32) {
    unsafe { write_volatile(BTABLE, value) };
}
pub fn write_daddr(value: u32) {
    unsafe { write_volatile(DADDR, value) };
}
pub fn clocks_48mhz() {
    clock_48mhz_impl();
}
fn clock_48mhz_impl() {
    let acr = 0x4002_2000usize as *mut u32;
    unsafe {
        write_volatile(acr, (read_volatile(acr) & !0b111) | 0b001 | (1 << 4));
        write_volatile(RCC_CR, read_volatile(RCC_CR) | CR_HSEON);
        while read_volatile(RCC_CR) & CR_HSERDY == 0 {}
        write_volatile(RCC_CFGR, (read_volatile(RCC_CFGR) & !CFGR_PLLMUL_MASK) | CFGR_PLLSRC_HSE | CFGR_PLLMUL_X6);
        write_volatile(RCC_CR, read_volatile(RCC_CR) | CR_PLLON);
        while read_volatile(RCC_CR) & CR_PLLRDY == 0 {}
        write_volatile(RCC_CFGR, (read_volatile(RCC_CFGR) & !CFGR_SW_MASK) | CFGR_SW_PLL);
        while read_volatile(RCC_CFGR) & CFGR_SWS_MASK != CFGR_SWS_PLL {}
        write_volatile(RCC_CFGR3, read_volatile(RCC_CFGR3) | CFGR3_USBSW_PLL);
        write_volatile(RCC_APB1ENR, read_volatile(RCC_APB1ENR) | RCC_APB1ENR_USBEN);
    }
}

pub fn pma_count_rx(n: usize) -> u16 {
    pma_r(bd(n) + 6)
}

pub fn write_volatile_u32(reg: *mut u32, value: u32) {
    unsafe { write_volatile(reg, value) };
}

/// Boot-stage breadcrumb for bring-up: stage number to SRAM 0x20000100
/// (above the data/bss end ~0x200000F0, far below the descending stack).
pub fn stage_trace(n: u32) {
    unsafe { core::ptr::write_volatile(0x2000_0100usize as *mut u32, n) };
}
