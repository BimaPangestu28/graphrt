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
    fn ids_are_unique() {
        let a = super::new_id(1);
        let b = super::new_id(1);
        assert_ne!(a, b);
    }
}
