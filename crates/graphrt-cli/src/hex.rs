//! Hex encoding for signatures and random ids.

/// Lowercase hex encoding.
pub fn to_hex(bytes: &[u8]) -> String {
    let digit = |nibble: u8| char::from_digit(u32::from(nibble), 16).unwrap_or('0');
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(digit(byte >> 4));
        out.push(digit(byte & 0x0f));
    }
    out
}

/// Decode hex (either case). `None` on odd length or a non-hex digit.
pub fn from_hex(text: &str) -> Option<Vec<u8>> {
    let digits = text.as_bytes();
    if !digits.len().is_multiple_of(2) {
        return None;
    }
    let value = |digit: u8| char::from(digit).to_digit(16).and_then(|v| u8::try_from(v).ok());
    digits.chunks_exact(2).map(|pair| Some(value(*pair.first()?)? << 4 | value(*pair.get(1)?)?)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        assert_eq!(from_hex("00ff7A"), Some(vec![0x00, 0xff, 0x7a]));
        assert_eq!(to_hex(&[0x00, 0xff, 0x7a]), "00ff7a");
        assert_eq!(from_hex("abc"), None);
        assert_eq!(from_hex("zz"), None);
    }
}
