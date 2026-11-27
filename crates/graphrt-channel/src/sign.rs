//! The checks of a webhook endpoint: comparing secrets in constant time and,
//! for WhatsApp, the HMAC-SHA256 signature of the raw body.

/// Whether `a` equals `b`, in time that depends on their lengths only, so a
/// caller cannot learn a secret byte by byte from how long a refusal takes.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0_u8, |difference, (x, y)| difference | (x ^ y)) == 0
}

/// Lower-case hex of `bytes`.
pub fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        for nibble in [byte >> 4, byte & 0x0f] {
            hex.push(char::from(DIGITS.get(usize::from(nibble)).copied().unwrap_or(b'0')));
        }
    }
    hex
}

/// The bytes a hex string names, or `None` when it is not hex.
pub fn from_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let digit = |byte: u8| char::from(byte).to_digit(16).and_then(|value| u8::try_from(value).ok());
    hex.as_bytes()
        .chunks(2)
        .map(|pair| match pair {
            [high, low] => Some((digit(*high)? << 4) | digit(*low)?),
            _ => None,
        })
        .collect()
}

/// HMAC-SHA256 (RFC 2104) of `message` under `key`.
#[cfg(feature = "whatsapp")]
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    /// SHA-256 block size in bytes.
    const BLOCK_BYTES: usize = 64;
    let hashed_key;
    let key = if key.len() > BLOCK_BYTES {
        hashed_key = Sha256::digest(key);
        hashed_key.as_slice()
    } else {
        key
    };
    let mut inner_pad = [0x36_u8; BLOCK_BYTES];
    let mut outer_pad = [0x5c_u8; BLOCK_BYTES];
    for ((inner, outer), key_byte) in inner_pad.iter_mut().zip(outer_pad.iter_mut()).zip(key) {
        *inner ^= key_byte;
        *outer ^= key_byte;
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_digest);
    outer.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_and_refuses_what_is_not_hex() {
        assert_eq!(to_hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
        assert_eq!(from_hex("000fA5ff"), Some(vec![0x00, 0x0f, 0xa5, 0xff]));
        assert_eq!(from_hex("abc"), None);
        assert_eq!(from_hex("zz"), None);
    }

    #[test]
    fn comparison_needs_equal_length_and_bytes() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret "));
        assert!(constant_time_eq(b"", b""));
    }

    #[cfg(feature = "whatsapp")]
    #[test]
    fn hmac_matches_the_rfc_4231_vectors() {
        let case_1 = hmac_sha256(&[0x0b; 20], b"Hi There");
        assert_eq!(to_hex(&case_1), "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7");
        let case_2 = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(to_hex(&case_2), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        // A key longer than a block is hashed first.
        let case_6 = hmac_sha256(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First");
        assert_eq!(to_hex(&case_6), "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54");
    }
}
