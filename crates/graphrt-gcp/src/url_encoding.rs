//! Percent-encoding of query and form values (RFC 3986 unreserved set kept).

/// Encodes `value` for a query string or an `application/x-www-form-urlencoded` body.
pub(crate) fn encode_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_unreserved_and_escapes_the_rest() {
        assert_eq!(encode_component("a-b.c_d~e"), "a-b.c_d~e");
        assert_eq!(encode_component("https://x.run.app/p?q=1"), "https%3A%2F%2Fx.run.app%2Fp%3Fq%3D1");
        assert_eq!(encode_component("urn:ietf:params"), "urn%3Aietf%3Aparams");
    }
}
