//! The token and cost budget of a run, and the checkpoints of running nodes.
//!
//! A run's budget ([`RunBudget`]) is one pair of counters shared by every
//! frame and every child run. Model nodes that track their turns
//! ([`crate::node::Node::tracks_turns`]) charge it after each model response
//! ([`crate::node::NodeContext::charge`]), with its cost, so an agent loop is
//! bounded per turn and a failed call is paid for too. The engine charges
//! every other node the tokens it reports when it completes. The counters and
//! the limits are in the snapshot and go on after resume.
//!
//! A checkpoint ([`NodeProgress`]) is the progress a running node keeps for
//! its next attempt: a retry after an error or a timeout gets it, and so does
//! the attempt that runs after the run was preempted and resumed.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::event::Usage;
use crate::node::NodeError;

/// A run's budget as its snapshot keeps it: what was charged, and the limits
/// the run started with (a request may have set lower ones than the engine's,
/// and a resumed run keeps them).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BudgetState {
    pub tokens: u64,
    /// In millionths of the price unit of the providers file.
    pub cost_micros: u64,
    pub max_total_tokens: Option<u64>,
    pub max_cost_micros: Option<u64>,
}

// Through `serde_json::Value`, whose code every build has already: a derive
// would add a decoder per input type to every binary that reads snapshots.
impl Serialize for BudgetState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut json = serde_json::Map::new();
        json.insert("tokens".into(), self.tokens.into());
        json.insert("cost_micros".into(), self.cost_micros.into());
        if let Some(max) = self.max_total_tokens {
            json.insert("max_total_tokens".into(), max.into());
        }
        if let Some(max) = self.max_cost_micros {
            json.insert("max_cost_micros".into(), max.into());
        }
        serde_json::Value::Object(json).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BudgetState {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let json = serde_json::Value::deserialize(deserializer)?;
        let number = |key: &str| -> Result<Option<u64>, D::Error> {
            match json.get(key) {
                None | Some(serde_json::Value::Null) => Ok(None),
                Some(value) => value
                    .as_u64()
                    .map(Some)
                    .ok_or_else(|| serde::de::Error::custom(format!("budget `{key}` must be a non-negative integer"))),
            }
        };
        if !json.is_object() {
            return Err(serde::de::Error::custom("budget must be an object"));
        }
        Ok(Self {
            tokens: number("tokens")?.unwrap_or(0),
            cost_micros: number("cost_micros")?.unwrap_or(0),
            max_total_tokens: number("max_total_tokens")?,
            max_cost_micros: number("max_cost_micros")?,
        })
    }
}

impl BudgetState {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The budget of one run: limits, and what was charged against them.
pub struct RunBudget {
    max_tokens: Option<u64>,
    max_cost_micros: Option<u64>,
    tokens: AtomicU64,
    cost_micros: AtomicU64,
    /// Called once with the reason when a charge passes a limit (the engine
    /// stops the run there, as at the step limit).
    on_exceeded: Box<dyn Fn(String) + Send + Sync>,
    exceeded: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for RunBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunBudget")
            .field("max_tokens", &self.max_tokens)
            .field("max_cost_micros", &self.max_cost_micros)
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

impl RunBudget {
    pub(crate) fn new(
        max_tokens: Option<u64>,
        max_cost_micros: Option<u64>,
        used: (u64, u64),
        on_exceeded: Box<dyn Fn(String) + Send + Sync>,
    ) -> Self {
        Self {
            max_tokens,
            max_cost_micros,
            tokens: AtomicU64::new(used.0),
            cost_micros: AtomicU64::new(used.1),
            on_exceeded,
            exceeded: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// A budget without limits that only counts, for node unit tests.
    pub fn unlimited() -> Self {
        Self::new(None, None, (0, 0), Box::new(|_| {}))
    }

    /// A budget with limits whose excess is only reported, for node unit tests.
    pub fn limited(max_tokens: Option<u64>, max_cost_micros: Option<u64>) -> Self {
        Self::new(max_tokens, max_cost_micros, (0, 0), Box::new(|_| {}))
    }

    /// Add `usage` (its total tokens) and `cost_micros`.
    ///
    /// @throws NodeError - `limit_exceeded` (fatal) when a limit is passed. The
    ///   run stops then, so the error ends the node that charged.
    pub fn charge(&self, usage: &Usage, cost_micros: u64) -> Result<(), NodeError> {
        let tokens = usage.billable_tokens();
        let used_tokens = saturating_add(&self.tokens, tokens);
        let used_cost = saturating_add(&self.cost_micros, cost_micros);
        let why = match (self.max_tokens, self.max_cost_micros) {
            (Some(max), _) if used_tokens > max => format!("token budget of {max} exceeded ({used_tokens} used)"),
            (_, Some(max)) if used_cost > max => {
                format!("cost budget of {max} micro units exceeded ({used_cost} used)")
            }
            _ => return Ok(()),
        };
        if !self.exceeded.swap(true, Ordering::AcqRel) {
            (self.on_exceeded)(why.clone());
        }
        Err(NodeError::fatal("limit_exceeded", why))
    }

    /// Add `usage` and `cost_micros` without checking the limits: what the
    /// last answer of a node cost when the node chose to answer within the
    /// budget (it may pass the limit by that one answer, never more).
    pub fn record(&self, usage: &Usage, cost_micros: u64) {
        saturating_add(&self.tokens, usage.billable_tokens());
        saturating_add(&self.cost_micros, cost_micros);
    }

    /// Stop the run when nothing is left of a limit, as a charge past it does.
    ///
    /// @throws NodeError - `limit_exceeded` (fatal) when the budget is spent.
    pub fn stop_if_spent(&self) -> Result<(), NodeError> {
        let spent = self.remaining_tokens() == Some(0) || self.remaining_cost_micros() == Some(0);
        if !spent {
            return Ok(());
        }
        let why = "the run's token or cost budget is spent".to_owned();
        if !self.exceeded.swap(true, Ordering::AcqRel) {
            (self.on_exceeded)(why.clone());
        }
        Err(NodeError::fatal("limit_exceeded", why))
    }

    /// Tokens left, `None` without a token limit.
    pub fn remaining_tokens(&self) -> Option<u64> {
        self.max_tokens.map(|max| max.saturating_sub(self.tokens.load(Ordering::Acquire)))
    }

    /// Cost left in micro units, `None` without a cost limit.
    pub fn remaining_cost_micros(&self) -> Option<u64> {
        self.max_cost_micros.map(|max| max.saturating_sub(self.cost_micros.load(Ordering::Acquire)))
    }

    /// Whether the run has a token or a cost limit.
    pub fn is_limited(&self) -> bool {
        self.max_tokens.is_some() || self.max_cost_micros.is_some()
    }

    /// What the snapshot keeps: the counters and the limits.
    pub fn state(&self) -> BudgetState {
        BudgetState {
            tokens: self.tokens.load(Ordering::Acquire),
            cost_micros: self.cost_micros.load(Ordering::Acquire),
            max_total_tokens: self.max_tokens,
            max_cost_micros: self.max_cost_micros,
        }
    }
}

/// Add `amount` to `counter` without wrapping. Returns the new value.
fn saturating_add(counter: &AtomicU64, amount: u64) -> u64 {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let next = current.saturating_add(amount);
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return next,
            Err(actual) => current = actual,
        }
    }
}

/// The checkpoint of one execution of a node that tracks its turns
/// ([`crate::node::Node::tracks_turns`]), shared by its attempts.
#[derive(Debug, Default)]
pub struct NodeProgress {
    checkpoint: Mutex<Option<serde_json::Value>>,
}

impl NodeProgress {
    #[cfg_attr(not(feature = "budgets"), allow(dead_code))]
    pub(crate) fn with(checkpoint: Option<serde_json::Value>) -> Self {
        Self { checkpoint: Mutex::new(checkpoint) }
    }

    pub(crate) fn set(&self, state: serde_json::Value) {
        *self.checkpoint.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(state);
    }

    pub(crate) fn get(&self) -> Option<serde_json::Value> {
        self.checkpoint.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn usage(total: u64) -> Usage {
        Usage { prompt_tokens: total, completion_tokens: 0, total_tokens: total }
    }

    #[test]
    fn a_charge_past_a_limit_reports_once_and_fails() {
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let log = std::sync::Arc::clone(&seen);
        let budget = RunBudget::new(Some(100), Some(50), (40, 0), Box::new(move |why| log.lock().unwrap().push(why)));
        budget.charge(&usage(50), 10).unwrap();
        assert_eq!(budget.remaining_tokens(), Some(10));
        assert_eq!(budget.remaining_cost_micros(), Some(40));
        let error = budget.charge(&usage(20), 0).unwrap_err();
        assert_eq!(error.kind, "limit_exceeded");
        assert!(!error.retryable);
        assert!(budget.charge(&usage(1), 0).is_err());
        assert_eq!(seen.lock().unwrap().len(), 1, "reported once");
        let state = budget.state();
        assert_eq!((state.tokens, state.cost_micros, state.max_total_tokens), (111, 10, Some(100)));
        // Counters never wrap.
        let huge = RunBudget::limited(Some(u64::MAX), None);
        huge.charge(&usage(u64::MAX - 1), 0).unwrap();
        assert!(huge.charge(&usage(10), 0).is_err() || huge.state().tokens == u64::MAX);
        assert_eq!(huge.state().tokens, u64::MAX);
        let costly = RunBudget::limited(None, Some(50));
        costly.charge(&usage(1_000), 50).unwrap();
        assert!(costly.charge(&usage(0), 1).unwrap_err().message.contains("cost budget of 50"));
        // Without limits it only counts.
        let open = RunBudget::unlimited();
        open.charge(&usage(1_000_000), 1_000_000).unwrap();
        assert_eq!((open.remaining_tokens(), open.is_limited()), (None, false));
    }

    #[test]
    fn progress_keeps_the_last_checkpoint() {
        let progress = NodeProgress::with(None);
        assert!(progress.get().is_none());
        progress.set(serde_json::json!({"turn": 1}));
        progress.set(serde_json::json!({"turn": 2}));
        assert_eq!(progress.get(), Some(serde_json::json!({"turn": 2})));
    }
}
