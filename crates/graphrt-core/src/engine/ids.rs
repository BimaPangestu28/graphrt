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
        h.write_u64(std::process::id().into());
        h.finish()
    })
}

/// A new identifier: `<time ms hex>-<per-process random hex>-<counter hex>`.
pub(crate) fn new_id(now_ms: u64) -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mixed = seed() ^ n.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    format!("{now_ms:011x}-{:08x}-{n:x}", mixed & 0xffff_ffff)
}

#[cfg(test)]
mod tests {
    #[test]
    fn ids_are_unique() {
        let a = super::new_id(1);
        let b = super::new_id(1);
        assert_ne!(a, b);
    }
}
