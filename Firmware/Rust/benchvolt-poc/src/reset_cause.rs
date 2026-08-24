//! STM32F0 RCC reset-cause decoding.

pub const OPTION_BYTE: u8 = 1 << 0;
pub const PIN: u8 = 1 << 1;
pub const POWER_ON: u8 = 1 << 2;
pub const SOFTWARE: u8 = 1 << 3;
pub const INDEPENDENT_WATCHDOG: u8 = 1 << 4;
pub const WINDOW_WATCHDOG: u8 = 1 << 5;
pub const LOW_POWER: u8 = 1 << 6;
pub const V18_DOMAIN: u8 = 1 << 7;

/// Compact the sticky RCC_CSR flags into a stable byte for diagnostics.
pub const fn decode_rcc_csr(csr: u32) -> u8 {
    ((csr >> 25) & 0x7f) as u8 | (((csr >> 23) & 1) as u8) << 7
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
}
