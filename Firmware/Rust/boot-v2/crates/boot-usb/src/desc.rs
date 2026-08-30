//! USB descriptors for the v2 boot cores — byte-for-byte the stock
//! bootloader's proven set (usbd_desc.c / usbd_cdc.c FS variants), whose
//! enumeration on this host and this board is field-proven. Identity
//! (VID/PID/strings) matches the stock CDC so the host's cached device
//! survives the v1→v2 handover without a re-enumeration dance.

// The stock identity is VID 1155 / PID 22336 in DECIMAL (usbd_desc.c) —
// 0x0483/0x5740, ST's real vendor ID.
pub const VID: u16 = 0x0483;
pub const PID_BOOT_CORE: u16 = 0x5740;

pub const EP0_PACKET: u8 = 64;
pub const BULK_PACKET: u8 = 64;

pub const STRING_INDEX_MANUFACTURER: u8 = 1;
pub const STRING_INDEX_PRODUCT: u8 = 2;

pub const DEVICE: [u8; 18] = [
    0x12, // bLength
    0x01, // DEVICE
    0x00, 0x02, // USB 2.0
    0x02, 0x00, 0x00, // class CDC, no subclass/protocol (stock style)
    0x40, // EP0 max packet 64
    0x83, 0x04, // VID 0x0483 (STMicroelectronics)
    0x40, 0x57, // PID 0x5740, identical to the stock bootloader
    0x00, 0x02, // device release 2.00
    1, 2, 3, 1, // manufacturer, product, serial, count
];

pub const CONFIG: [u8; 67] = [
    9, 0x02, 67, 0, 2, 1, 0, 0xC0, 50, // configuration (self-powered, 100 mA)
    9, 0x04, 0, 0, 1, 0x02, 0x02, 0x01, 0, // interface 0: communication
    5, 0x24, 0x00, 0x10, 0x01, // header bcdCDC 1.10
    5, 0x24, 0x01, 0x00, 0x01, // call management
    4, 0x24, 0x02, 0x02, // ACM (capabilities byte-identical to stock)
    5, 0x24, 0x06, 0x00, 0x01, // union: master 0, data 1
    7, 0x05, 0x82, 0x03, 0x08, 0x00, 0x20, // EP2 IN, interrupt, 8 B, 32 ms
    9, 0x04, 1, 0, 2, 0x0A, 0x00, 0x00, 0, // interface 1: data
    7, 0x05, 0x81, 0x02, 0x40, 0x00, 0x00, // EP1 IN bulk 64
    7, 0x05, 0x01, 0x02, 0x40, 0x00, 0x00, // EP1 OUT bulk 64
];

pub const STRING_LANGID: [u8; 4] = [4, 0x03, 0x09, 0x04];

// Exactly bLength bytes each. Serving a padded fixed-size buffer instead
// hangs EP0 when the host asks with wLength > the real length: the final
// packet is then a full 64 bytes, the host expects a continuation, and the
// device has none to give.
pub const STRING_MANUFACTURER: [u8; 38] = string_const("STMicroelectronics");
pub const STRING_PRODUCT: [u8; 44] = string_const("STM32 Virtual ComPort");
pub const STRING_SERIAL: [u8; 36] = string_const("BENCHVOLT-V2-BOOT");

pub const fn string_const<const N: usize>(text: &str) -> [u8; N] {
    assert!(N == 2 + 2 * text.len());
    let mut out = [0u8; N];
    out[0] = N as u8;
    out[1] = 0x03;
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        out[2 + 2 * index] = bytes[index];
        index += 1;
    }
    out
}
