//! `/metrics` in the Prometheus text format, from atomic counters (no
//! metrics crate). Labels never carry secrets or user ids (I6).

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

/// Gateway-wide counters.
#[derive(Debug, Default)]
pub struct Metrics {
    /// Runs accepted.
    pub runs_started: AtomicU64,
    /// Runs that reached a terminal status.
    pub runs_finished: AtomicU64,
    /// Runs paused (HITL).
    pub runs_paused: AtomicU64,
    /// Re-dispatches after drain / preemption / a lost replica.
    pub redispatches: AtomicU64,
    /// Executions lost without an outcome and re-dispatched from a checkpoint
    /// or from the start (nodes may have run twice).
    pub lost_executions: AtomicU64,
    /// Requests refused by auth.
    pub auth_failures: AtomicU64,
    /// Requests refused by a rate limit.
    pub rate_limited: AtomicU64,
    /// Requests refused because no replica became available.
    pub activation_failures: AtomicU64,
    /// Webhook deliveries accepted / duplicates.
    pub webhooks_accepted: AtomicU64,
    /// Duplicate webhook deliveries answered from the marker.
    pub webhooks_duplicate: AtomicU64,
    /// Cron fires started.
    pub cron_fires: AtomicU64,
    /// Cron fires skipped with a recorded reason.
    pub cron_skipped: AtomicU64,
    /// Callbacks delivered / failed attempts.
    pub callbacks_delivered: AtomicU64,
    /// Callback attempts that failed.
    pub callbacks_failed: AtomicU64,
    /// Store write failures.
    pub store_write_errors: AtomicU64,
    /// Client SSE streams open.
    pub client_streams: AtomicU64,
}

/// Increments a counter.
pub fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Per-deployment gauges supplied at render time.
#[derive(Debug, Clone, Default)]
pub struct DeploymentGauges {
    /// Deployment name.
    pub deployment: String,
    /// Version.
    pub version: String,
    /// Replicas.
    pub replicas: u32,
    /// Runs holding a slot.
    pub active_runs: u32,
    /// Requests waiting.
    pub queued: u32,
    /// Prewarm counters.
    pub prewarm: crate::pool::PrewarmStats,
}

impl Metrics {
    /// Renders the exposition text.
    pub fn render(&self, gauges: &[DeploymentGauges], live_runs: usize) -> String {
        let mut out = String::with_capacity(2048);
        let counters: [(&str, &AtomicU64, &str); 16] = [
            ("graphrt_gateway_runs_started_total", &self.runs_started, "Runs accepted."),
            ("graphrt_gateway_runs_finished_total", &self.runs_finished, "Runs that reached a terminal status."),
            ("graphrt_gateway_runs_paused_total", &self.runs_paused, "Runs paused for human input."),
            (
                "graphrt_gateway_redispatches_total",
                &self.redispatches,
                "Runs re-dispatched after drain, preemption or a lost replica.",
            ),
            (
                "graphrt_gateway_lost_executions_total",
                &self.lost_executions,
                "Executions lost without an outcome and re-dispatched from a checkpoint or the start.",
            ),
            ("graphrt_gateway_auth_failures_total", &self.auth_failures, "Requests refused by authentication."),
            ("graphrt_gateway_rate_limited_total", &self.rate_limited, "Requests refused by a rate limit."),
            (
                "graphrt_gateway_activation_failures_total",
                &self.activation_failures,
                "Requests refused because no replica became available.",
            ),
            ("graphrt_gateway_webhooks_accepted_total", &self.webhooks_accepted, "Webhook deliveries accepted."),
            ("graphrt_gateway_webhooks_duplicate_total", &self.webhooks_duplicate, "Duplicate webhook deliveries."),
            ("graphrt_gateway_cron_fires_total", &self.cron_fires, "Cron fires started."),
            ("graphrt_gateway_cron_skipped_total", &self.cron_skipped, "Cron fires skipped with a recorded reason."),
            ("graphrt_gateway_callbacks_delivered_total", &self.callbacks_delivered, "Completion callbacks delivered."),
            (
                "graphrt_gateway_callbacks_failed_total",
                &self.callbacks_failed,
                "Completion callback attempts that failed.",
            ),
            ("graphrt_gateway_store_write_errors_total", &self.store_write_errors, "Store writes that failed."),
            ("graphrt_gateway_client_streams", &self.client_streams, "Client SSE streams open."),
        ];
        for (name, counter, help) in counters {
            let kind = if name.ends_with("_total") { "counter" } else { "gauge" };
            let _infallible =
                writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {}", counter.load(Ordering::Relaxed));
        }
        let _infallible = writeln!(
            out,
            "# HELP graphrt_gateway_live_runs Runs whose stream the gateway holds.\n# TYPE graphrt_gateway_live_runs gauge\ngraphrt_gateway_live_runs {live_runs}"
        );
        for (name, help) in [
            ("replicas", "Replicas per deployment version."),
            ("active_runs", "Runs holding a replica slot."),
            ("queued", "Requests waiting for a replica."),
        ] {
            let _infallible =
                writeln!(out, "# HELP graphrt_gateway_{name} {help}\n# TYPE graphrt_gateway_{name} gauge");
            for gauge in gauges {
                let value = match name {
                    "replicas" => gauge.replicas,
                    "active_runs" => gauge.active_runs,
                    _ => gauge.queued,
                };
                let _infallible = writeln!(
                    out,
                    "graphrt_gateway_{name}{{deployment=\"{}\",version=\"{}\"}} {value}",
                    escape(&gauge.deployment),
                    escape(&gauge.version)
                );
            }
        }
        render_prewarm(&mut out, gauges);
        out
    }
}

/// Prewarm counters per deployment version and source. Hit rate:
/// `sum(rate(…_prewarm_hits_total[1d])) / sum(rate(…_prewarm_started_total[1d]))`.
fn render_prewarm(out: &mut String, gauges: &[DeploymentGauges]) {
    use crate::warm::PrewarmSource;
    for (name, help) in [
        ("prewarm_started_total", "Replicas started by a prewarm."),
        ("prewarm_hits_total", "Prewarmed replicas whose first lease came (traffic found them warm)."),
        ("prewarm_misses_total", "Prewarmed replicas released or lost without a lease."),
    ] {
        let _infallible = writeln!(out, "# HELP graphrt_gateway_{name} {help}\n# TYPE graphrt_gateway_{name} counter");
        for gauge in gauges {
            let values = match name {
                "prewarm_started_total" => gauge.prewarm.started,
                "prewarm_hits_total" => gauge.prewarm.hits,
                _ => gauge.prewarm.misses,
            };
            for source in PrewarmSource::ALL {
                let _infallible = writeln!(
                    out,
                    "graphrt_gateway_{name}{{deployment=\"{}\",version=\"{}\",source=\"{}\"}} {}",
                    escape(&gauge.deployment),
                    escape(&gauge.version),
                    source.label(),
                    source.value(values)
                );
            }
        }
    }
}

/// Keep-warm budgets `(deployment, used today, limit)` in replica-seconds, and the wake gate.
pub fn render_warm(out: &mut String, budgets: &[(String, u64, u64)], gate: Option<&crate::warm::WakeGate>) {
    if !budgets.is_empty() {
        let _infallible = writeln!(
            out,
            "# HELP graphrt_gateway_warm_replica_seconds Idle replica-seconds kept warm today (UTC).\n# TYPE graphrt_gateway_warm_replica_seconds gauge"
        );
        for (deployment, used, _) in budgets {
            let _infallible =
                writeln!(out, "graphrt_gateway_warm_replica_seconds{{deployment=\"{}\"}} {used}", escape(deployment));
        }
        let _infallible = writeln!(
            out,
            "# HELP graphrt_gateway_warm_budget_replica_seconds Daily keep-warm budget.\n# TYPE graphrt_gateway_warm_budget_replica_seconds gauge"
        );
        for (deployment, _, limit) in budgets {
            let _infallible = writeln!(
                out,
                "graphrt_gateway_warm_budget_replica_seconds{{deployment=\"{}\"}} {limit}",
                escape(deployment)
            );
        }
    }
    if let Some(gate) = gate {
        let _infallible = writeln!(
            out,
            "# HELP graphrt_gateway_wakes_in_flight Provider wakes in flight.\n# TYPE graphrt_gateway_wakes_in_flight gauge\ngraphrt_gateway_wakes_in_flight {}\n# HELP graphrt_gateway_wakes_gated_total Wakes that waited for the max_parallel_wakes gate.\n# TYPE graphrt_gateway_wakes_gated_total counter\ngraphrt_gateway_wakes_gated_total {}",
            gate.in_flight(),
            gate.gated.load(Ordering::Relaxed)
        );
    }
}

fn escape(label: &str) -> String {
    label.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_counters_and_gauges() {
        let metrics = Metrics::default();
        bump(&metrics.runs_started);
        let text = metrics.render(
            &[DeploymentGauges {
                deployment: "shop".into(),
                version: "v1".into(),
                replicas: 2,
                active_runs: 3,
                queued: 0,
                prewarm: crate::pool::PrewarmStats { started: [2, 0, 0, 0], hits: [1, 0, 0, 0], ..Default::default() },
            }],
            3,
        );
        assert!(text.contains(
            "graphrt_gateway_prewarm_hits_total{deployment=\"shop\",version=\"v1\",source=\"predictive\"} 1"
        ));
        assert!(text.contains(
            "graphrt_gateway_prewarm_started_total{deployment=\"shop\",version=\"v1\",source=\"predictive\"} 2"
        ));
        assert!(text.contains("graphrt_gateway_runs_started_total 1"));
        assert!(text.contains("graphrt_gateway_replicas{deployment=\"shop\",version=\"v1\"} 2"));
        assert!(text.contains("graphrt_gateway_live_runs 3"));
    }
}
