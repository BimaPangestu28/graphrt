//! Lowercase hex encoding.

use std::fmt::Write as _;

/// Lowercase hex of `bytes`.
pub fn encode(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
        let _infallible = write!(out, "{byte:02x}");
        out
    })
}

/// Bytes of an even-length hex string.
pub fn decode(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    let digit = |c: u8| (c as char).to_digit(16).and_then(|value| u8::try_from(value).ok());
    text.as_bytes().chunks(2).map(|pair| Some((digit(*pair.first()?)? << 4) | digit(*pair.get(1)?)?)).collect()
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    encode(&sha2::Sha256::digest(bytes))
}

/// Compares two byte strings in time independent of where they differ.
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && left.iter().zip(right).fold(0u8, |difference, (a, b)| difference | (a ^ b)) == 0
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trips() {
        assert_eq!(super::encode(&[0, 171, 255]), "00abff");
        assert_eq!(super::decode("00abff"), Some(vec![0, 171, 255]));
        assert_eq!(super::decode("0g"), None);
        assert_eq!(super::decode("abc"), None);
    }
}
