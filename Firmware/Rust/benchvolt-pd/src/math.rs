//! Bit-exact 64-by-32-bit division. Every u64 division in this firmware has a
//! divisor that fits in u32; using the general `u64 / u64` operator links
//! compiler_builtins' ~0.9 KiB software divider on thumbv6m. This shift-
//! subtract loop is a fraction of that size, and the call rate (a few per
//! millisecond at worst) makes its extra cycles irrelevant at 48 MHz.

/// Returns `(numerator / divisor, numerator % divisor)`, identical to the
/// native operators for any nonzero `divisor`.
pub fn div_rem_u64(numerator: u64, divisor: u64) -> (u64, u64) {
    debug_assert!(divisor != 0);
    let mut quotient = 0u64;
    let mut remainder = 0u64;
    let bits = 64 - numerator.leading_zeros();
    for bit in (0..bits).rev() {
        remainder = (remainder << 1) | ((numerator >> bit) & 1);
        quotient <<= 1;
        if remainder >= divisor {
            remainder -= divisor;
            quotient |= 1;
        }
    }
    (quotient, remainder)
}

#[cfg(test)]
mod tests {
    use super::div_rem_u64;

    #[test]
    fn matches_native_division_across_magnitudes() {
        let numerators = [
            0u64,
            1,
            99,
            100,
            2_000_000,
            u64::from(u32::MAX),
            u64::from(u32::MAX) + 1,
            (1_999_999u64 << 32) + 1_999_999,
            u64::MAX,
        ];
        let divisors = [
            1u64,
            2,
            100,
            2_000_000,
            u64::from(u32::MAX),
            u64::from(u32::MAX) + 5,
            u64::MAX,
        ];
        for numerator in numerators {
            for divisor in divisors {
                let (quotient, remainder) = div_rem_u64(numerator, divisor);
                assert_eq!(quotient, numerator / divisor);
                assert_eq!(remainder, numerator % divisor);
            }
        }
    }
}
