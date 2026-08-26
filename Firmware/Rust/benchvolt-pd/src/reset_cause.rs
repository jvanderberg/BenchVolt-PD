//! STM32F0 RCC reset-cause decoding.

pub const OPTION_BYTE: u8 = 1 << 0;
pub const PIN: u8 = 1 << 1;
pub const POWER_ON: u8 = 1 << 2;
pub const SOFTWARE: u8 = 1 << 3;
pub const INDEPENDENT_WATCHDOG: u8 = 1 << 4;
pub const WINDOW_WATCHDOG: u8 = 1 << 5;
pub const LOW_POWER: u8 = 1 << 6;
pub const V18_DOMAIN: u8 = 1 << 7;
pub const MARKER_TAG: u32 = 0xB056_5200;
const MARKER_TAG_MASK: u32 = 0xffff_ff00;

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResetReason {
    Panic = 1,
    HardFault = 2,
    AdcInitialization = 3,
    UserReboot = 4,
    BootloaderRequest = 5,
    WatchdogConfiguration = 6,
}

impl ResetReason {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Panic),
            2 => Some(Self::HardFault),
            3 => Some(Self::AdcInitialization),
            4 => Some(Self::UserReboot),
            5 => Some(Self::BootloaderRequest),
            6 => Some(Self::WatchdogConfiguration),
            _ => None,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Panic => "PANIC",
            Self::HardFault => "HARDFAULT",
            Self::AdcInitialization => "ADC_INIT",
            Self::UserReboot => "USER",
            Self::BootloaderRequest => "BOOTLOADER",
            Self::WatchdogConfiguration => "IWDG_CONFIG",
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ResetMarker {
    pub code: u32,
    pub inverse: u32,
}

impl ResetMarker {
    pub const fn new(reason: ResetReason) -> Self {
        let code = MARKER_TAG | reason as u32;
        Self {
            code,
            inverse: !code,
        }
    }

    pub const fn decode(self, reset_causes: u8) -> Option<ResetReason> {
        if reset_causes & (POWER_ON | V18_DOMAIN) != 0
            || self.code & MARKER_TAG_MASK != MARKER_TAG
            || self.inverse != !self.code
        {
            return None;
        }
        ResetReason::from_raw(self.code & !MARKER_TAG_MASK)
    }
}

/// Compact the sticky RCC_CSR flags into a stable byte for diagnostics.
pub const fn decode_rcc_csr(csr: u32) -> u8 {
    ((csr >> 25) & 0x7f) as u8 | (((csr >> 23) & 1) as u8) << 7
}

/// Reading untouched SRAM can raise an NMI when the option-byte parity check
/// is enabled. A retained marker is therefore best-effort unless parity is
/// disabled, and is never trusted across a power-domain reset.
pub const fn retained_marker_read_allowed(reset_causes: u8, ram_parity_disabled: bool) -> bool {
    ram_parity_disabled && reset_causes & (POWER_ON | V18_DOMAIN) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_individual_and_overlapping_reset_flags() {
        assert_eq!(decode_rcc_csr(1 << 25), OPTION_BYTE);
        assert_eq!(decode_rcc_csr(1 << 29), INDEPENDENT_WATCHDOG);
        assert_eq!(decode_rcc_csr(1 << 23), V18_DOMAIN);
        assert_eq!(decode_rcc_csr((1 << 26) | (1 << 27)), PIN | POWER_ON);
    }

    #[test]
    fn ignores_oscillator_and_clear_control_bits() {
        assert_eq!(decode_rcc_csr(0x0100_0003), 0);
    }

    #[test]
    fn retained_marker_requires_all_redundant_fields() {
        let marker = ResetMarker::new(ResetReason::HardFault);
        assert_eq!(marker.decode(SOFTWARE), Some(ResetReason::HardFault));

        let mut corrupt = marker;
        corrupt.inverse ^= 1;
        assert_eq!(corrupt.decode(SOFTWARE), None);
        corrupt = marker;
        corrupt.code ^= 1 << 8;
        assert_eq!(corrupt.decode(SOFTWARE), None);
    }

    #[test]
    fn power_domain_reset_never_trusts_retained_ram() {
        let marker = ResetMarker::new(ResetReason::Panic);
        assert_eq!(marker.decode(POWER_ON), None);
        assert_eq!(marker.decode(V18_DOMAIN), None);
        assert_eq!(marker.decode(POWER_ON | SOFTWARE), None);
        assert!(retained_marker_read_allowed(SOFTWARE, true));
        assert!(!retained_marker_read_allowed(SOFTWARE, false));
        assert!(!retained_marker_read_allowed(POWER_ON, true));
    }
}
