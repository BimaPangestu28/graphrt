//! Process-unique identifiers for runs and node executions, without a UUID dependency.

use std::cell::RefCell;
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
    let mut id = String::with_capacity(ID_CAPACITY);
    write_id(&mut id, now_ms);
    id
}

/// Longest id: 16 + 1 + 8 + 1 + 16 hex digits and separators.
const ID_CAPACITY: usize = 42;

fn write_id(out: &mut String, now_ms: u64) {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mixed = seed() ^ n.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    push_hex(out, now_ms, 11);
    out.push('-');
    push_hex(out, mixed & 0xffff_ffff, 8);
    out.push('-');
    push_hex(out, n, 1);
}

/// [`new_id`] directly as an `Arc<str>`, minted once per node execution.
///
/// The digits go into a reused per-thread `String`, so the id costs only the
/// `Arc` allocation. Building it as `char`s keeps the buffer valid UTF-8 by
/// construction; the earlier byte buffer needed a `from_utf8` pass per id,
/// which was a measurable share of a node's cost.
pub(crate) fn new_id_arc(now_ms: u64) -> std::sync::Arc<str> {
    thread_local! {
        static SCRATCH: RefCell<String> = const { RefCell::new(String::new()) };
    }
    SCRATCH.with(|scratch| match scratch.try_borrow_mut() {
        Ok(mut buf) => {
            buf.clear();
            buf.reserve(ID_CAPACITY);
            write_id(&mut buf, now_ms);
            std::sync::Arc::from(buf.as_str())
        }
        // Not reentrant in practice; fall back rather than panic.
        Err(_) => std::sync::Arc::from(new_id(now_ms)),
    })
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
