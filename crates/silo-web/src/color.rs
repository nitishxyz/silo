//! Page colours as reported by the theme script: `#rrggbb` (the script
//! normalizes any CSS colour), or empty for none.

/// `0x00RRGGBB`, or `None` for an empty or malformed report.
#[must_use]
pub fn parse(value: &str) -> Option<u32> {
    let hex = value.strip_prefix('#').filter(|h| h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit()))?;
    u32::from_str_radix(hex, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn reports() {
        assert_eq!(parse("#0b3d2e"), Some(0x0b3d2e));
        assert_eq!(parse("#0B3D2E"), Some(0x0b3d2e));
        assert_eq!(parse("#000000"), Some(0));
        assert_eq!(parse(""), None, "no colour");
        assert_eq!(parse("#fff"), None);
        assert_eq!(parse("#0b3d2e00"), None);
        assert_eq!(parse("0b3d2e"), None);
        assert_eq!(parse("#+b3d2e"), None);
        assert_eq!(parse("rgb(0, 0, 0)"), None);
    }
}
