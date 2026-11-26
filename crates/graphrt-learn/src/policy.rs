//! The learning policy of an agent: limits, and what may be approved without a person.

use serde::{Deserialize, Serialize};

use crate::lesson::{Risk, Trust};

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// Limits and automatic approval for one agent's lessons.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningPolicy {
    /// Approve a lesson without a person when all of these hold: it is the
    /// direct feedback of an authenticated user, it applies only to that
    /// user, and its risk is low. Off by default: every lesson waits for the
    /// owner.
    #[serde(default)]
    pub auto_approve_user_feedback: bool,
    /// Most approved lessons for the agent as a whole, and for each user.
    #[serde(default = "default_max_lessons")]
    pub max_lessons_per_scope: usize,
    /// Most proposals waiting for a decision, per agent.
    #[serde(default = "default_max_proposals")]
    pub max_open_proposals: usize,
    /// Most characters of lessons placed in one run's prompt.
    #[serde(default = "default_max_note_chars")]
    pub max_note_chars: usize,
    /// Days after which a proposal lapses without a decision.
    #[serde(default = "default_proposal_ttl_days")]
    pub proposal_ttl_days: u32,
}

fn default_max_lessons() -> usize {
    100
}

fn default_max_proposals() -> usize {
    50
}

fn default_max_note_chars() -> usize {
    4_000
}

fn default_proposal_ttl_days() -> u32 {
    30
}

impl Default for LearningPolicy {
    fn default() -> Self {
        Self {
            auto_approve_user_feedback: false,
            max_lessons_per_scope: default_max_lessons(),
            max_open_proposals: default_max_proposals(),
            max_note_chars: default_max_note_chars(),
            proposal_ttl_days: default_proposal_ttl_days(),
        }
    }
}

impl LearningPolicy {
    /// Whether a new lesson may be approved without a person.
    ///
    /// `own_scope` says the lesson applies only to the user whose words it is.
    /// A lesson derived from run content never qualifies, whatever the policy.
    pub fn approves_automatically(&self, trust: Trust, risk: Risk, own_scope: bool) -> bool {
        self.auto_approve_user_feedback && trust == Trust::User && risk == Risk::Low && own_scope
    }

    /// When a proposal made at `now_ms` lapses.
    pub fn proposal_expiry(&self, now_ms: u64) -> u64 {
        now_ms.saturating_add(u64::from(self.proposal_ttl_days).saturating_mul(DAY_MS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_approved_automatically_by_default() {
        let policy = LearningPolicy::default();
        for trust in [Trust::Owner, Trust::User, Trust::Derived] {
            assert!(!policy.approves_automatically(trust, Risk::Low, true));
        }
    }

    #[test]
    fn automatic_approval_needs_every_condition() {
        let policy = LearningPolicy { auto_approve_user_feedback: true, ..LearningPolicy::default() };
        assert!(policy.approves_automatically(Trust::User, Risk::Low, true));
        assert!(!policy.approves_automatically(Trust::User, Risk::High, true), "high risk waits for the owner");
        assert!(!policy.approves_automatically(Trust::User, Risk::Low, false), "another scope waits for the owner");
        assert!(
            !policy.approves_automatically(Trust::Derived, Risk::Low, true),
            "derived content never approves itself"
        );
    }

    #[test]
    fn missing_fields_take_their_defaults() {
        let policy: LearningPolicy = serde_json::from_str("{}")
            .unwrap_or_else(|_| LearningPolicy { max_lessons_per_scope: 0, ..LearningPolicy::default() });
        assert_eq!(policy, LearningPolicy::default());
        assert_eq!(policy.proposal_expiry(1_000), 1_000 + 30 * DAY_MS);
    }
}
