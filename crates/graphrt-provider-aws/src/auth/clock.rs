//! Wall clock with a learned skew correction.
//!
//! SigV4 rejects requests whose `x-amz-date` is more than 5 minutes off. When AWS
//! answers with a skew error the REST client records the server's `Date` and
//! every later signature uses the corrected time (conformance C8).

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Shared clock; clones share the skew offset.
#[derive(Clone, Debug, Default)]
pub struct Clock {
    offset_secs: Arc<AtomicI64>,
    /// Fixed base time for tests (`None` = system time).
    fixed_unix: Option<u64>,
}

impl Clock {
    /// A clock pinned to `unix_seconds` (tests).
    pub fn fixed(unix_seconds: u64) -> Self {
        Self { offset_secs: Arc::default(), fixed_unix: Some(unix_seconds) }
    }

    fn base(&self) -> u64 {
        self.fixed_unix.unwrap_or_else(|| SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()))
    }

    /// Corrected Unix time in seconds.
    pub fn now_unix(&self) -> u64 {
        let base = i64::try_from(self.base()).unwrap_or(i64::MAX);
        u64::try_from(base.saturating_add(self.offset_secs.load(Ordering::Relaxed))).unwrap_or(0)
    }

    /// Current correction in seconds (server − local).
    pub fn offset_secs(&self) -> i64 {
        self.offset_secs.load(Ordering::Relaxed)
    }

    /// Learn the offset from a server timestamp (Unix seconds).
    pub fn correct_to(&self, server_unix: u64) {
        let base = i64::try_from(self.base()).unwrap_or(i64::MAX);
        let server = i64::try_from(server_unix).unwrap_or(i64::MAX);
        self.offset_secs.store(server.saturating_sub(base), Ordering::Relaxed);
    }
}

/// Parse an RFC 7231 HTTP date (`Sun, 06 Nov 1994 08:49:37 GMT`) into Unix seconds.
pub(crate) fn parse_http_date(value: &str) -> Option<u64> {
    let mut parts = value.split_whitespace().skip(1);
    let day: u32 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts.next()?.parse().ok()?;
    let time = parts.next()?;
    super::sts::parse_iso8601(&format!("{year:04}-{month:02}-{day:02}T{time}Z"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skew_correction_shifts_now() {
        let clock = Clock::fixed(1_000_000);
        clock.correct_to(1_000_400);
        assert_eq!(clock.now_unix(), 1_000_400);
        assert_eq!(clock.clone().offset_secs(), 400);
        clock.correct_to(999_700);
        assert_eq!(clock.now_unix(), 999_700);
    }

    #[test]
    fn http_date_parses() {
        assert_eq!(parse_http_date("Sun, 30 Aug 2015 12:36:00 GMT"), Some(1_440_938_160));
        assert_eq!(parse_http_date("garbage"), None);
    }
}
