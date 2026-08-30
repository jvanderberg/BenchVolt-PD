#![no_std]
//! Polled raw-register USB CDC device stack for the STM32F070.
//!
//! Mirrors the stock bootloader's proven CDC configuration exactly
//! (`usbd_conf.c:334-340`, `usbd_cdc.h:44-46`): EP0 control (PMA 0x18/0x58),
//! EP1 bulk IN (0xC0) + bulk OUT (0x110), EP2 interrupt IN (0x100, STALLed).
//! No interrupts, no allocator; the owner polls `poll()`.

pub mod ctrl;
pub mod desc;
pub mod regs;

use regs::{
    ep_modify, ep_read, pma_read_bytes, pma_write_bytes, set_stat_rx, set_stat_tx, write_btable,
    write_cntr, write_daddr,
};

pub const EP_CONTROL: usize = 0;
pub const EP_BULK: usize = 1;
pub const EP_INT: usize = 2;

pub const EP0_TX_BUF: u16 = 0x18;
pub const EP0_RX_BUF: u16 = 0x58;
pub const EP1_TX_BUF: u16 = 0xC0;
pub const EP1_RX_BUF: u16 = 0x110;
pub const EP2_TX_BUF: u16 = 0x100;

pub const BD_TX_ADDR: usize = 0;
pub const BD_TX_COUNT: usize = 2;
pub const BD_RX_ADDR: usize = 4;
pub const BD_RX_COUNT: usize = 6;

pub const MAX_PACKET: usize = 64;
/// COUNT_RX for a 64-byte buffer: BL_SIZE=1 (32-byte blocks), NUM_BLOCK=1
/// → (1+1)*32 = 64, the same encoding the HAL's PCD_SET_EP_RX_CNT computes.
pub const COUNT_RX_64: u16 = (1 << 15) | (1 << 10);

pub const DESC_DEVICE: u8 = 1;
pub const DESC_CONFIG: u8 = 2;
pub const DESC_STRING: u8 = 3;

pub const GET_STATUS: u8 = 0x00;
pub const SET_ADDRESS: u8 = 0x05;
pub const GET_DESCRIPTOR: u8 = 0x06;
pub const SET_CONFIGURATION: u8 = 0x09;
pub const SET_INTERFACE: u8 = 0x0B;
pub const SET_LINE_CODING: u8 = 0x20;
pub const GET_LINE_CODING: u8 = 0x21;
pub const SET_CONTROL_LINE_STATE: u8 = 0x22;

pub const LINE_CODING: [u8; 7] = [0x00, 0xC2, 0x01, 0x00, 0x00, 0x00, 0x08];

/// GET_STATUS response: self-powered, no remote wakeup, no error.
pub const STATUS_DATA: [u8; 2] = [0x00, 0x00];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Desc {
    Device,
    Config,
    LangId,
    Manufacturer,
    Product,
    Serial,
    Status,
    LineCoding,
}

pub fn desc_bytes(which: Desc) -> &'static [u8] {
    match which {
        Desc::Device => &desc::DEVICE,
        Desc::Config => &desc::CONFIG,
        Desc::LangId => &desc::STRING_LANGID,
        Desc::Manufacturer => &desc::STRING_MANUFACTURER,
        Desc::Product => &desc::STRING_PRODUCT,
        Desc::Serial => unsafe { &*core::ptr::addr_of!(desc::STRING_SERIAL) },
        Desc::Status => &STATUS_DATA,
        Desc::LineCoding => &LINE_CODING,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Idle,
    TxIn { which: Desc, total: usize },
    StatusIn { address: u8 },
    RxOut,
    StatusOut,
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Event {
    None,
    /// Bulk-OUT packet received; retrieve with `recv()`.
    BulkRx,
}

pub struct Usb {
    pub(crate) stage: Stage,
    /// (descriptor source, bytes already queued) for the EP0 IN data stage.
    pub(crate) tx: Option<(Desc, usize)>,
    pub(crate) bulk_rx_len: usize,
    pub(crate) last_chunk_len: usize,
    pub configured: bool,
}

impl Usb {
    /// Configure clocks (48 MHz) and the USB peripheral; arms EP0 and the
    /// bulk-OUT endpoint. Blocks until HSE/PLL are stable.
    pub fn new() -> Self {
        crate::regs::stage_trace(1);
        desc::init_serial();
        regs::clocks_48mhz();
        crate::regs::stage_trace(2);
        // Host-visible disconnect FIRST: the stock bootloader enables the
        // pull-up during its screen time, so without dropping it here the
        // host believes the old CDC device is still attached and frames the
        // bus forever without re-enumerating. Hold D+ low ~2 s: the host is
        // usually mid-enumeration of the stock bootloader's device at the
        // handover instant, and a short blip re-attaches before it finishes
        // tearing that device down — macOS then counts enumeration failures
        // against the port and starts abandoning attach attempts after
        // SET_ADDRESS. Feed a possibly-armed IWDG while waiting.
        regs::write_volatile_u32(regs::BCDR, 0);
        for _ in 0..8 {
            unsafe { core::ptr::write_volatile(0x4000_3000usize as *mut u32, 0xaaaa) };
            regs::spin(12_000_000); // ~250 ms at 48 MHz
        }
        crate::regs::stage_trace(2);
        // Init sequence: HAL USB_DevInit (ll_usb.c:154-173), the proven
        // sequence on this silicon. The first write also clears PDWN (set
        // after a cold reset); tSTARTUP (~1 µs) must elapse before FRES is
        // released so the analog transceiver is powered.
        write_cntr(regs::CNTR_FRES);
        regs::spin(100);
        write_cntr(0);
        regs::write_istr(0); // rc_w0: writing 0 clears every flag
        write_btable(0);
        crate::regs::stage_trace(3);
        let mut usb = Usb { stage: Stage::Idle, tx: None, bulk_rx_len: 0, last_chunk_len: 0, configured: false };
        usb.endpoints_init();
        crate::regs::stage_trace(4);
        write_daddr(regs::DADDR_EF);
        write_cntr(regs::CNTR_CTRM | regs::CNTR_RESETM);
        crate::regs::stage_trace(5);
        // D+ 1.5 kΩ pull-up: the host only enumerates once this is set
        // (the stock bootloader does the same; main.c:488). No artificial
        // delays — the IWDG (armed by a previous v1 session) forbids any
        // long pre-loop stall without feeding.
        regs::write_volatile_u32(regs::BCDR, regs::BCDR_DPPU);
        crate::regs::stage_trace(6);
        usb
    }

    /// (Re)initialize buffer descriptors and endpoint registers; also used
    /// on bus reset.
    pub fn endpoints_init(&mut self) {
        write_btable(0);
        // Buffer descriptor entries (standard layout: ADDR_TX @ +0,
        // COUNT_TX @ +2, ADDR_RX @ +4, COUNT_RX @ +6).
        regs::pma_half(regs::bd(EP_CONTROL) + BD_RX_ADDR, EP0_RX_BUF);
        regs::pma_half(regs::bd(EP_CONTROL) + BD_RX_COUNT, COUNT_RX_64);
        regs::pma_half(regs::bd(EP_CONTROL) + BD_TX_ADDR, EP0_TX_BUF);
        regs::pma_half(regs::bd(EP_CONTROL) + BD_TX_COUNT, 0);
        regs::pma_half(regs::bd(EP_BULK) + BD_RX_ADDR, EP1_RX_BUF);
        regs::pma_half(regs::bd(EP_BULK) + BD_RX_COUNT, COUNT_RX_64);
        regs::pma_half(regs::bd(EP_BULK) + BD_TX_ADDR, EP1_TX_BUF);
        regs::pma_half(regs::bd(EP_BULK) + BD_TX_COUNT, 0);
        regs::pma_half(regs::bd(EP_INT) + BD_TX_ADDR, EP2_TX_BUF);
        regs::pma_half(regs::bd(EP_INT) + BD_TX_COUNT, 0);
        crate::regs::stage_trace(11);
        // EP0 control, TX NAK / RX VALID (awaiting SETUP); EP2 interrupt IN
        // NAK (nothing to notify — STALL would mark the endpoint halted and
        // can make the host's CDC driver bail); EP1 bulk, TX NAK / RX VALID.
        // The EP_TYPE write matters: after handover the EPnRs are zeroed
        // (type = BULK), and a control endpoint that is left as BULK never
        // receives the host's SETUP.
        regs::ep_init(EP_CONTROL, regs::EP_TYPE_CONTROL, regs::STAT_NAK, regs::STAT_VALID);
        regs::ep_init(EP_INT, regs::EP_TYPE_INTERRUPT, regs::STAT_NAK, regs::STAT_DISABLED);
        regs::ep_init(EP_BULK, regs::EP_TYPE_BULK, regs::STAT_NAK, regs::STAT_VALID);
        self.stage = Stage::Idle;
        self.tx = None;
        self.bulk_rx_len = 0;
        self.last_chunk_len = 0;
    }

    /// Poll the peripheral: handles bus reset and EP0 control traffic;
    /// returns `Event::BulkRx` when a bulk-OUT packet awaits `recv()`.
    pub fn poll(&mut self) -> Event {
        let istr = regs::read_istr();
        if istr & regs::ISTR_RESET != 0 {
            // Bus-reset counter at 0x20000104 (bring-up diagnostics).
            unsafe {
                let c = core::ptr::read_volatile(0x2000_0104usize as *mut u32);
                core::ptr::write_volatile(0x2000_0104usize as *mut u32, c + 1);
            }
            regs::write_istr(!regs::ISTR_RESET);
            self.configured = false;
            self.endpoints_init();
            // A bus reset clears DADDR; without re-enabling EF the device
            // never answers the host's address-0 traffic again.
            write_daddr(regs::DADDR_EF);
            return Event::None;
        }

        // CTR_TX strictly BEFORE CTR_RX, with a fresh read for each: a
        // pending tx-done always belongs to the transfer in progress, while
        // a pending SETUP starts a new one. Handling them from one stale
        // read in the other order let a finished transfer's CTR_TX advance
        // the next transfer's freshly-queued first chunk — the host then
        // read chunk 2 as the first packet of every back-to-back
        // multi-packet control transfer (how the 67-byte config-descriptor
        // read died on hardware).
        if ep_read(EP_CONTROL) & regs::EP_CTR_TX != 0 {
            self.ep0_tx_done();
        }
        let ep0 = ep_read(EP_CONTROL);
        if ep0 & regs::EP_CTR_RX != 0 {
            ep_modify(EP_CONTROL, None, None, true, false);
            if ep0 & regs::EP_SETUP != 0 {
                self.ep0_setup();
            } else {
                self.ep0_rx_out();
            }
        }

        if self.bulk_rx_len == 0 {
            let epr = ep_read(EP_BULK);
            if epr & regs::EP_CTR_RX != 0 {
                // Consume the event first; STAT_RX sits in NAK until
                // rx_release(), so no second packet can land while this one
                // is processed. COUNT_RX needs a settle: the engine raises
                // CTR_RX marginally before the count is visible in the PMA,
                // and this polled loop reads faster than any ISR ever would
                // — observed on hardware as the PREVIOUS packet's count on
                // the final (size-changing) chunk of an upload, ~1 in 12
                // runs. Spin ~1 µs, then require two equal reads.
                ep_modify(EP_BULK, None, None, true, false);
                regs::spin(100);
                let mut count = regs::pma_count_rx(EP_BULK) & 0x03FF;
                for _ in 0..1_000 {
                    let again = regs::pma_count_rx(EP_BULK) & 0x03FF;
                    if again == count {
                        break;
                    }
                    count = again;
                }
                self.bulk_rx_len = count as usize;
                return Event::BulkRx;
            }
        }

        // Reset-end wipe guard: the hardware wipes the EPnRs (and DADDR) at
        // the END of bus-reset signaling, which can land after our
        // RESET-flag handling above. A wiped EP1R reads EA == 0, which the
        // armed state never does — use that as the detector and redo the
        // whole post-reset init (ep_init is state-independent, so this is
        // safe even on a false positive at any instant).
        if ep_read(EP_BULK) & regs::EP_ADDR_FIELD == 0 {
            self.configured = false;
            self.endpoints_init();
        }
        if unsafe { core::ptr::read_volatile(regs::DADDR) } & regs::DADDR_EF == 0 {
            write_daddr(regs::DADDR_EF);
        }
        // Belt-and-suspenders: EP0 must never sit with reception disabled
        // (SETUP would be lost) and EP1 must never sit fully disabled.
        if ep_read(EP_CONTROL) & regs::EP_STAT_RX == 0 {
            regs::ep_init(EP_CONTROL, regs::EP_TYPE_CONTROL, regs::STAT_NAK, regs::STAT_VALID);
            self.stage = Stage::Idle;
            self.tx = None;
        }
        let ep1 = ep_read(EP_BULK);
        if regs::stat_tx_of(ep1) == regs::STAT_DISABLED {
            set_stat_tx(EP_BULK, regs::STAT_NAK);
        }
        if ep1 & regs::EP_STAT_RX == 0 && self.bulk_rx_len == 0 {
            set_stat_rx(EP_BULK, regs::STAT_VALID);
        }
        Event::None
    }

    /// Copy a received bulk-OUT packet (≤64 bytes). Reception stays NAKed
    /// until `rx_release()` — call it once the packet is fully processed.
    pub fn recv(&mut self, out: &mut [u8]) -> usize {
        let count = self.bulk_rx_len.min(out.len());
        pma_read_bytes(EP1_RX_BUF, &mut out[..count]);
        self.bulk_rx_len = 0;
        count
    }

    /// Re-arm bulk-OUT reception after the current packet's command has
    /// been handled. Hardware NAKs the host in between — correct USB flow
    /// control, and it keeps the event/count reads race-free.
    pub fn rx_release(&mut self) {
        set_stat_rx(EP_BULK, regs::STAT_VALID);
    }

    /// Queue a bulk-IN packet (≤64 bytes). `poll()` must be called for it
    /// to drain; `bulk_tx_idle()` reports completion.
    pub fn send(&mut self, data: &[u8]) {
        let n = data.len().min(MAX_PACKET);
        pma_write_bytes(EP1_TX_BUF, &data[..n]);
        regs::pma_half(regs::bd(EP_BULK) + BD_TX_COUNT, n as u16);
        set_stat_tx(EP_BULK, regs::STAT_VALID);
    }

    /// True when no bulk-IN transfer is in flight.
    pub fn bulk_tx_idle(&self) -> bool {
        regs::stat_tx_of(ep_read(EP_BULK)) == regs::STAT_NAK
    }
}
