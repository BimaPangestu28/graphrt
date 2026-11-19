//! Process-unique identifiers for runs and node executions, without a UUID dependency.

use std::hash::{BuildHasher, Hasher};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);
static SEED: OnceLock<u64> = OnceLock::new();

fn seed() -> u64 {
    *SEED.get_or_init(|| {
        // `RandomState` is randomly keyed per process by the standard library.
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        // `std::process::id` is unsupported (panics) on wasm32-unknown-unknown.
        #[cfg(not(target_arch = "wasm32"))]
        let pid = u64::from(std::process::id());
        #[cfg(target_arch = "wasm32")]
        let pid = 0_u64;
        h.write_u64(pid);
        h.finish()
    })
}

/// A new identifier: `<time ms hex>-<per-process random hex>-<counter hex>`.
///
/// One is minted per run and per node execution, so the hex digits are written
/// directly instead of through `format!` (whose padding machinery showed up in
/// engine profiles). The output is identical to
/// `format!("{now_ms:011x}-{:08x}-{n:x}", mixed & 0xffff_ffff)`.
pub(crate) fn new_id(now_ms: u64) -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mixed = seed() ^ n.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let mut id = String::with_capacity(16 + 1 + 8 + 1 + 16);
    push_hex(&mut id, now_ms, 11);
    id.push('-');
    push_hex(&mut id, mixed & 0xffff_ffff, 8);
    id.push('-');
    push_hex(&mut id, n, 1);
    id
}

/// [`new_id`] directly as an `Arc<str>`: written into a stack buffer, so minting
/// an execution id costs one allocation instead of two.
pub(crate) fn new_id_arc(now_ms: u64) -> std::sync::Arc<str> {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mixed = seed() ^ n.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let mut buf = HexBuf::default();
    buf.hex(now_ms, 11);
    buf.byte(b'-');
    buf.hex(mixed & 0xffff_ffff, 8);
    buf.byte(b'-');
    buf.hex(n, 1);
    std::sync::Arc::from(buf.as_str())
}

/// Fixed-size ASCII buffer for ids (11 + 1 + 8 + 1 + 16 hex digits at most).
struct HexBuf {
    bytes: [u8; 48],
    len: usize,
}

impl Default for HexBuf {
    fn default() -> Self {
        Self { bytes: [0; 48], len: 0 }
    }
}

impl HexBuf {
    fn byte(&mut self, b: u8) {
        if let Some(slot) = self.bytes.get_mut(self.len) {
            *slot = b;
            self.len += 1;
        }
    }

    fn hex(&mut self, value: u64, min_width: usize) {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let digits = (64 - value.leading_zeros()).div_ceil(4).max(1) as usize;
        for _ in digits..min_width {
            self.byte(b'0');
        }
        for i in (0..digits).rev() {
            let nibble = (value >> (i * 4)) & 0xf;
            self.byte(DIGITS.get(nibble as usize).copied().unwrap_or(b'0'));
        }
    }

    fn as_str(&self) -> &str {
        self.bytes.get(..self.len).and_then(|b| std::str::from_utf8(b).ok()).unwrap_or_default()
    }
}

/// Append `value` in lowercase hex, left-padded with zeros to `min_width` digits.
fn push_hex(out: &mut String, value: u64, min_width: usize) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let digits = (64 - value.leading_zeros()).div_ceil(4).max(1) as usize;
    for _ in digits..min_width {
        out.push('0');
    }
    for i in (0..digits).rev() {
        let nibble = (value >> (i * 4)) & 0xf;
        out.push(char::from(DIGITS.get(nibble as usize).copied().unwrap_or(b'0')));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn hex_matches_format() {
        for value in [0u64, 1, 0xf, 0x10, 0xabc, 0x1234_5678, u64::from(u32::MAX), u64::MAX, 1_700_000_000_000] {
            for width in [1usize, 8, 11] {
                let mut out = String::new();
                super::push_hex(&mut out, value, width);
                assert_eq!(out, format!("{value:0width$x}"), "value {value:#x}, width {width}");
            }
        }
    }

    #[test]
    fn arc_ids_have_the_string_id_format() {
        let id = super::new_id_arc(0x1_2345);
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 3, "{id}");
        assert_eq!(parts[0], "00000012345");
        assert_eq!(parts[1].len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'), "{id}");
        assert_ne!(super::new_id_arc(1), super::new_id_arc(1));
    }

    #[test]
    fn ids_are_unique() {
        let a = super::new_id(1);
        let b = super::new_id(1);
        assert_ne!(a, b);
    }
}
