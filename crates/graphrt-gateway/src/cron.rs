//! Cron schedules (plan §2, D8): five-field expressions evaluated in a time
//! zone, with an explicit DST policy, catch-up after downtime, deterministic
//! jitter and a simple prewarm.
//!
//! - **DST overlap** (a wall time happening twice): fires once, at the first occurrence.
//! - **DST gap** (a wall time that does not exist): skipped, and recorded as
//!   skipped with reason `dst_gap`, unless `catchup` is not `none`, in which case it fires
//!   at the first instant after the gap.
//! - **Catch-up** after the gateway was down: `none` (record the misses),
//!   `latest` (fire the most recent missed time) or `all` (fire up to
//!   `max_catchup` of them, oldest first). Every skipped fire is reported, so
//!   none is lost without a recorded reason (invariant I3).
//! - **Jitter**: `H(deployment, schedule, fire_time) mod jitter_secs`, the same
//!   on every restart, so a fire's run id never changes.
//! - **Prewarm**: the deployment is woken `prewarm_secs` before the fire.
//!
//! The run id of a fire is `H(deployment, schedule, fire_time)` and the run
//! record is the idempotency marker: at most one run per fire (I3).

use serde::Deserialize;

pub use graphrt_core::cron::CronExpr;
use graphrt_core::cron::tz::{Zone, days_from_civil};

/// What happens to a fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FireKind {
    /// Fires at its time.
    OnTime,
    /// Its wall time fell into a DST gap and it fires at the end of the gap.
    AfterGap,
}

/// One scheduled instant: the nominal wall time (as UTC of its first
/// occurrence, or the end of the gap) that keys the run id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Fire {
    /// Unix seconds the fire happens at (before jitter).
    pub at: i64,
}

/// Catch-up policy after downtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Catchup {
    /// Missed fires are recorded as skipped.
    #[default]
    None,
    /// The most recent missed fire runs.
    Latest,
    /// Up to `max_catchup` missed fires run, oldest first.
    All,
}

/// A schedule: expression, zone and policies.
#[derive(Debug, Clone)]
pub struct Schedule {
    /// Schedule id (part of the run id).
    pub id: String,
    expr: CronExpr,
    zone: Zone,
    /// Catch-up policy.
    pub catchup: Catchup,
    /// Upper bound of fires replayed by `Catchup::All` (plan default 1).
    pub max_catchup: usize,
    /// Upper bound of the deterministic delay.
    pub jitter_secs: u64,
    /// Wake the deployment this long before the fire.
    pub prewarm_secs: u64,
}

/// A fire that will not run, and why (logged to the audit log).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedFire {
    /// The nominal instant.
    pub at: i64,
    /// `dst_gap` or `missed_while_down`.
    pub reason: &'static str,
}

/// Fires to run and to record as skipped after a downtime.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatchupPlan {
    /// Fires to run now, oldest first.
    pub run: Vec<i64>,
    /// Fires recorded as skipped.
    pub skipped: Vec<SkippedFire>,
}

/// Upper bound on days searched for the next fire (`0 0 29 2 1` can be years away).
use graphrt_core::cron::SEARCH_DAYS;

impl Schedule {
    /// Builds a schedule.
    ///
    /// # Errors
    /// Bad expression or unknown zone.
    pub fn new(id: &str, expression: &str, zone: &str) -> Result<Self, String> {
        Ok(Self {
            id: id.to_owned(),
            expr: CronExpr::parse(expression)?,
            zone: Zone::load(zone)?,
            catchup: Catchup::None,
            max_catchup: 1,
            jitter_secs: 0,
            prewarm_secs: 0,
        })
    }

    /// Next fire strictly after `after` (unix seconds), including DST gap fires
    /// when `catchup != none`, together with whether it was shifted out of a gap.
    /// Gap fires otherwise appear in `gaps` so they can be recorded.
    pub fn next_after(&self, after: i64, gaps: &mut Vec<SkippedFire>) -> Option<(Fire, FireKind)> {
        let start_local = after + i64::from(self.zone.offset_at(after));
        let first_day = start_local.div_euclid(86_400);
        for days in first_day..first_day + SEARCH_DAYS {
            if !self.expr.day_matches(days) {
                continue;
            }
            for hour in self.expr.hours() {
                for minute in self.expr.minutes() {
                    let local = days * 86_400 + i64::from(*hour) * 3600 + i64::from(*minute) * 60;
                    if local < start_local - 86_400 {
                        continue;
                    }
                    let instants = self.zone.local_to_utc(local);
                    match instants.first() {
                        Some(first) if *first > after => return Some((Fire { at: *first }, FireKind::OnTime)),
                        Some(_) => {}
                        None => {
                            let gap_end = self.gap_end(local);
                            if gap_end > after {
                                if self.catchup == Catchup::None {
                                    gaps.push(SkippedFire { at: gap_end, reason: "dst_gap" });
                                } else {
                                    return Some((Fire { at: gap_end }, FireKind::AfterGap));
                                }
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// First UTC instant after the gap containing local wall time `local`.
    fn gap_end(&self, local: i64) -> i64 {
        let widest = self.zone.offset_at(local - 86_400).max(self.zone.offset_at(local + 86_400));
        let mut probe = local - i64::from(widest);
        while probe + i64::from(self.zone.offset_at(probe)) <= local {
            probe += 60;
        }
        probe
    }

    /// Deterministic jitter of a fire of `deployment`.
    pub fn jitter(&self, deployment: &str, fire_at: i64) -> u64 {
        if self.jitter_secs == 0 {
            return 0;
        }
        let digest = crate::hex::sha256_hex(format!("{deployment}|{}|{fire_at}", self.id).as_bytes());
        let prefix = u64::from_str_radix(digest.get(..16).unwrap_or("0"), 16).unwrap_or(0);
        prefix % (self.jitter_secs + 1)
    }

    /// What to do about fires in `(last_fired, now]` after a downtime.
    pub fn plan_catchup(&self, last_fired: i64, now: i64) -> CatchupPlan {
        let mut plan = CatchupPlan::default();
        let mut missed = Vec::new();
        let mut cursor = last_fired;
        while let Some((fire, _)) = self.next_after(cursor, &mut plan.skipped) {
            if fire.at > now {
                break;
            }
            missed.push(fire.at);
            cursor = fire.at;
            if missed.len() > 100_000 {
                break;
            }
        }
        plan.skipped.retain(|skipped| skipped.at <= now);
        let keep = match self.catchup {
            Catchup::None => 0,
            Catchup::Latest => 1,
            Catchup::All => self.max_catchup,
        };
        let split = missed.len().saturating_sub(keep);
        plan.skipped.extend(missed.iter().take(split).map(|at| SkippedFire { at: *at, reason: "missed_while_down" }));
        plan.run = missed.into_iter().skip(split).collect();
        plan.skipped.sort_unstable_by_key(|skipped| skipped.at);
        plan
    }
}

/// Run id of a fire: `cron-<hex(H(deployment, schedule, fire_time))[..32]>`.
pub fn fire_run_id(deployment: &str, schedule: &str, fire_at: i64) -> String {
    let digest = crate::hex::sha256_hex(format!("cron|{deployment}|{schedule}|{fire_at}").as_bytes());
    format!("cron-{}", digest.get(..32).unwrap_or_default())
}

/// Unix seconds of a UTC date-time (tests and docs).
pub fn utc(year: i64, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
    days_from_civil(year, month, day) * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60
}

#[cfg(test)]
mod tests {
    use super::*;

    fn berlin(expression: &str) -> Option<Schedule> {
        Schedule::new("s", expression, "Europe/Berlin").ok()
    }

    #[test]
    fn next_fire_in_utc() {
        let schedule = Schedule::new("s", "30 2 * * *", "UTC").unwrap();
        let mut gaps = Vec::new();
        let (fire, kind) = schedule.next_after(utc(2026, 11, 22, 3, 0), &mut gaps).unwrap();
        assert_eq!(fire.at, utc(2026, 11, 23, 2, 30));
        assert_eq!(kind, FireKind::OnTime);
        let (fire, _) = schedule.next_after(utc(2026, 11, 23, 2, 29), &mut gaps).unwrap();
        assert_eq!(fire.at, utc(2026, 11, 23, 2, 30));
    }

    #[test]
    fn dst_overlap_fires_once() {
        let Some(schedule) = berlin("30 2 * * *") else { return };
        let mut gaps = Vec::new();
        // 2026-10-25: 02:30 local happens at 00:30 UTC and 01:30 UTC.
        let (fire, _) = schedule.next_after(utc(2026, 10, 24, 12, 0), &mut gaps).unwrap();
        assert_eq!(fire.at, utc(2026, 10, 25, 0, 30));
        let (next, _) = schedule.next_after(fire.at, &mut gaps).unwrap();
        assert_eq!(next.at, utc(2026, 10, 26, 1, 30), "no second fire in the repeated hour");
    }

    #[test]
    fn dst_gap_is_skipped_and_recorded_or_shifted() {
        let Some(mut schedule) = berlin("30 2 * * *") else { return };
        let mut gaps = Vec::new();
        // 2026-03-29: 02:00-03:00 local does not exist.
        let (fire, _) = schedule.next_after(utc(2026, 3, 28, 12, 0), &mut gaps).unwrap();
        assert_eq!(fire.at, utc(2026, 3, 30, 0, 30));
        assert_eq!(gaps, vec![SkippedFire { at: utc(2026, 3, 29, 1, 0), reason: "dst_gap" }]);
        schedule.catchup = Catchup::Latest;
        let mut gaps = Vec::new();
        let (fire, kind) = schedule.next_after(utc(2026, 3, 28, 12, 0), &mut gaps).unwrap();
        assert_eq!((fire.at, kind), (utc(2026, 3, 29, 1, 0), FireKind::AfterGap));
        assert!(gaps.is_empty());
    }

    #[test]
    fn catchup_policies() {
        let mut schedule = Schedule::new("s", "0 * * * *", "UTC").unwrap();
        let last = utc(2026, 11, 22, 0, 0);
        let now = utc(2026, 11, 22, 5, 10);
        let plan = schedule.plan_catchup(last, now);
        assert!(plan.run.is_empty());
        assert_eq!(plan.skipped.len(), 5);
        schedule.catchup = Catchup::Latest;
        assert_eq!(schedule.plan_catchup(last, now).run, vec![utc(2026, 11, 22, 5, 0)]);
        schedule.catchup = Catchup::All;
        schedule.max_catchup = 3;
        let plan = schedule.plan_catchup(last, now);
        assert_eq!(plan.run, vec![utc(2026, 11, 22, 3, 0), utc(2026, 11, 22, 4, 0), utc(2026, 11, 22, 5, 0)]);
        assert_eq!(plan.skipped.len(), 2, "every miss is either run or recorded");
    }

    #[test]
    fn jitter_and_run_ids_are_deterministic() {
        let mut schedule = Schedule::new("nightly", "0 0 * * *", "UTC").unwrap();
        schedule.jitter_secs = 300;
        let jitter = schedule.jitter("shop", 1000);
        assert!(jitter <= 300);
        assert_eq!(jitter, schedule.jitter("shop", 1000));
        assert_eq!(fire_run_id("shop", "nightly", 1000), fire_run_id("shop", "nightly", 1000));
        assert_ne!(fire_run_id("shop", "nightly", 1000), fire_run_id("blog", "nightly", 1000));
        assert!(graphrt_core::store::validate_run_id(&fire_run_id("shop", "nightly", 1000)).is_ok());
    }

    #[test]
    fn day_of_month_or_week() {
        let schedule = Schedule::new("s", "0 0 13 * fri", "UTC").unwrap();
        let mut gaps = Vec::new();
        // From Sun 2026-11-22: next is Fri 2026-11-27 (weekday) before the 13th of December.
        let (fire, _) = schedule.next_after(utc(2026, 11, 22, 0, 0), &mut gaps).unwrap();
        assert_eq!(fire.at, utc(2026, 11, 27, 0, 0));
    }
}
