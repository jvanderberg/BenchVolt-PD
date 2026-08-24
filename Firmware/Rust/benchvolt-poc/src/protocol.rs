/// Parse a non-negative decimal value into thousandths without floating point.
///
/// At most three fractional digits are accepted, and all overflow is rejected.
pub fn parse_milliunits(text: &[u8]) -> Option<u16> {
    let (whole, fraction) = match text.iter().position(|byte| *byte == b'.') {
        Some(dot) => (&text[..dot], &text[dot + 1..]),
        None => (text, &[][..]),
    };
    if whole.is_empty()
        || whole.iter().any(|byte| !byte.is_ascii_digit())
        || fraction.len() > 3
        || fraction.iter().any(|byte| !byte.is_ascii_digit())
    {
        return None;
    }
    let whole = whole.iter().try_fold(0u32, |value, byte| {
        value.checked_mul(10)?.checked_add(u32::from(*byte - b'0'))
    })?;
    let mut fractional = 0u32;
    for byte in fraction {
        fractional = fractional * 10 + u32::from(*byte - b'0');
    }
    for _ in fraction.len()..3 {
        fractional *= 10;
    }
    u16::try_from(whole.checked_mul(1_000)?.checked_add(fractional)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_parser_preserves_exact_thousandths() {
        assert_eq!(parse_milliunits(b"0"), Some(0));
        assert_eq!(parse_milliunits(b"1.2"), Some(1_200));
        assert_eq!(parse_milliunits(b"3.045"), Some(3_045));
        assert_eq!(parse_milliunits(b"65.535"), Some(u16::MAX));
    }

    #[test]
    fn decimal_parser_rejects_malformed_and_overflowing_values() {
        for invalid in [
            b"" as &[u8],
            b".5",
            b"1.0000",
            b"1.2.3",
            b"-1",
            b"65.536",
            b"999999999999999999999999",
        ] {
            assert_eq!(parse_milliunits(invalid), None, "{invalid:?}");
        }
    }
}
