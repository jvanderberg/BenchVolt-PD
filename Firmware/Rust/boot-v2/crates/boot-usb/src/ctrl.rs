//! EP0 control-transfer state machine for `boot-usb`.
//!
//! Handled requests (mirrors what the stock CDC stack answers):
//! standard SET_ADDRESS / GET_DESCRIPTOR / SET_CONFIGURATION /
//! SET_INTERFACE, and CDC class SET_LINE_CODING (OUT swallowed),
//! GET_LINE_CODING (fixed 115200-8-N-1), SET_CONTROL_LINE_STATE.

use super::{
    desc_bytes, ep_modify, pma_read_bytes, pma_write_bytes, regs, set_stat_rx, set_stat_tx,
    write_daddr, Desc, Stage, Usb, BD_TX_COUNT, EP_CONTROL, EP0_TX_BUF,
};

impl Usb {
    /// Arm the status-IN stage: a true ZLP. COUNT_TX must be zeroed here —
    /// leaving the previous data stage's count sends stale bytes where the
    /// host expects a zero-length packet, which makes it fail the request
    /// (SET_CONFIGURATION "succeeding" on the device while the host drops
    /// the configuration is exactly this).
    fn arm_status_in(&mut self, address: u8) {
        self.stage = Stage::StatusIn { address };
        regs::pma_half(regs::bd(EP_CONTROL) + BD_TX_COUNT, 0);
        set_stat_tx(EP_CONTROL, regs::STAT_VALID);
    }

    /// SETUP received: clear CTR_RX, parse, dispatch.
    pub(crate) fn ep0_setup(&mut self) {
        // A SETUP aborts whatever control transfer was in flight.
        self.tx = None;
        let mut packet = [0u8; 8];
        pma_read_bytes(super::EP0_RX_BUF, &mut packet);
        let request = packet[1];
        let wlength = u16::from_le_bytes([packet[6], packet[7]]) as usize;

        match (packet[0], request) {
            (0x00, super::SET_ADDRESS) => {
                self.arm_status_in(packet[2]);
            }
            (0x00, super::SET_CONFIGURATION) => {
                self.configured = packet[2] != 0;
                self.arm_status_in(0);
            }
            (0x00, super::SET_INTERFACE) => {
                self.arm_status_in(0);
            }
            (0x80, super::GET_STATUS) => {
                // Remote wakeup not supported: two zero bytes.
                self.stage = Stage::TxIn { which: Desc::Status, total: 2 };
                self.tx = Some((Desc::Status, 0));
                self.tx_chunk(2);
            }
            (0x80, super::GET_DESCRIPTOR) => {
                let which = match packet[3] {
                    super::DESC_DEVICE => Some(Desc::Device),
                    super::DESC_CONFIG => Some(Desc::Config),
                    super::DESC_STRING => match packet[2] {
                        0 => Some(Desc::LangId),
                        1 => Some(Desc::Manufacturer),
                        2 => Some(Desc::Product),
                        3 => Some(Desc::Serial),
                        _ => None,
                    },
                    _ => None,
                };
                match which {
                    Some(which) if wlength > 0 => self.begin_tx(which, wlength),
                    _ => self.stall_ep0(),
                }
            }
            (0x21, super::SET_LINE_CODING) => {
                self.stage = Stage::RxOut;
                set_stat_rx(EP_CONTROL, regs::STAT_VALID);
            }
            (0xA1, super::GET_LINE_CODING) => {
                self.begin_tx(Desc::LineCoding, 7);
            }
            (0x21, super::SET_CONTROL_LINE_STATE) => {
                self.arm_status_in(0);
            }
            _ => self.stall_ep0(),
        }
    }

    fn stall_ep0(&mut self) {
        self.stage = Stage::Idle;
        self.tx = None;
        set_stat_tx(EP_CONTROL, regs::STAT_STALL);
        set_stat_rx(EP_CONTROL, regs::STAT_STALL);
    }

    /// OUT data stage on EP0 (SET_LINE_CODING payload or a status ZLP).
    pub(crate) fn ep0_rx_out(&mut self) {
        match self.stage {
            Stage::RxOut => {
                // Payload consumed in place; answer with status IN.
                self.arm_status_in(0);
            }
            Stage::StatusOut => {
                self.stage = Stage::Idle;
                ep_modify(EP_CONTROL, Some(regs::STAT_NAK), Some(regs::STAT_VALID), false, false);
            }
            _ => {
                set_stat_rx(EP_CONTROL, regs::STAT_VALID);
            }
        }
    }

    fn begin_tx(&mut self, which: Desc, wlength: usize) {
        let total = desc_bytes(which).len().min(wlength);
        self.stage = Stage::TxIn { which, total };
        self.tx = Some((which, 0));
        self.tx_chunk(64.min(total));
    }

    fn tx_chunk(&mut self, chunk: usize) {
        let (which, sent) = match self.tx {
            Some(x) => x,
            None => return,
        };
        let bytes = desc_bytes(which);
        let start = sent.min(bytes.len());
        let end = (sent + chunk).min(bytes.len());
        pma_write_bytes(EP0_TX_BUF, &bytes[start..end]);
        regs::pma_half(regs::bd(EP_CONTROL) + BD_TX_COUNT, (end - start) as u16);
        self.last_chunk_len = end - start;
        set_stat_tx(EP_CONTROL, regs::STAT_VALID);
    }

    /// CTR_TX on EP0: advance the control data/status stage.
    pub(crate) fn ep0_tx_done(&mut self) {
        ep_modify(EP_CONTROL, None, None, false, true);
        match self.stage {
            Stage::TxIn { which, total } => {
                let sent = self.tx.map(|(_, s)| s).unwrap_or(0) + self.last_chunk_len;
                self.tx = Some((which, sent));
                if sent >= total {
                    self.stage = Stage::StatusOut;
                    self.tx = None;
                    set_stat_rx(EP_CONTROL, regs::STAT_VALID);
                } else {
                    self.tx_chunk(64.min(total - sent));
                }
            }
            Stage::StatusIn { address } => {
                if address != 0 {
                    write_daddr((address as u32 & 0x7F) | regs::DADDR_EF);
                }
                self.stage = Stage::Idle;
                ep_modify(EP_CONTROL, Some(regs::STAT_NAK), Some(regs::STAT_VALID), false, false);
            }
            _ => {}
        }
    }
}
