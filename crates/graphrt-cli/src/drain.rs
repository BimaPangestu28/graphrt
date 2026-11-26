//! Graceful shutdown of `graphrt serve`: the drain that runs on SIGTERM / SIGINT.
//!
//! Platforms stop a container with SIGTERM (Fly's default is SIGINT) and kill
//! it a grace period later. When graphrt is PID 1 of a container (Docker,
//! Kubernetes, Cloud Run: the image has no init), the kernel ignores a signal
//! the process installed no handler for, so without one a SIGTERM did nothing
//! and the process lost every active run to the SIGKILL. The drain:
//!
//! 1. stops accepting runs: run starts, resumes and webhook deliveries get
//!    `503` with `Retry-After`, `/readyz` reports `503 draining` (`/healthz`
//!    stays `200`);
//! 2. pauses every active run (async, streaming and blocking alike): no node
//!    starts any more, in-flight nodes may finish;
//! 3. preempts the runs still active near the deadline ([`preempt_at`]):
//!    in-flight nodes are cancelled and saved as pending, to run again on resume
//!    (at-least-once per node, same `exec_id`);
//! 4. waits until every run stored its final state (`paused` with a snapshot,
//!    or finished), then lets the HTTP server close its connections and exits,
//!    all within `--drain-timeout`.
//!
//! A drained run is `paused` with a pause reason of kind [`DRAINED`]. Streaming
//! and blocking callers receive it (`run_paused` / `"status": "paused"`) and
//! resume it themselves; async and webhook runs, which no caller waits for, are
//! resumed by the next server that starts on the same store (see
//! [`is_auto_resumable`]). A second signal during the drain preempts at once.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::Either;
use graphrt_core::event::PauseReason;
use graphrt_core::store::RunRecord;
use graphrt_core::{Snapshot, Value};

use crate::error::CliError;
use crate::serve::AppState;

/// Pause reason kind of runs paused by a shutdown drain.
pub const DRAINED: &str = "drained";

/// `data.resume` of a drained run that the next server resumes by itself.
const RESUME_AUTO: &str = "auto";
/// `data.resume` of a drained run whose caller (stream or blocking request) resumes it.
const RESUME_CLIENT: &str = "client";

/// How often the drain checks on the active runs.
const POLL: Duration = Duration::from_millis(10);

/// Parse a duration: `500ms`, `4s`, `2m`, or plain (possibly fractional) seconds.
///
/// @param text - The duration text (`&str`)
/// @returns The duration (`Duration`)
/// @throws String - When the text is not a non-negative number with an optional unit
///
/// @example
/// assert_eq!(parse_duration("1500ms"), Ok(Duration::from_millis(1500)));
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let (number, scale) = if let Some(n) = text.strip_suffix("ms") {
        (n, 0.001)
    } else if let Some(n) = text.strip_suffix('s') {
        (n, 1.0)
    } else if let Some(n) = text.strip_suffix('m') {
        (n, 60.0)
    } else {
        (text, 1.0)
    };
    let seconds = number
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n >= 0.0)
        .ok_or_else(|| format!("invalid duration `{text}` (use e.g. 500ms, 4s, 2m)"))?;
    Duration::try_from_secs_f64(seconds * scale).map_err(|e| format!("invalid duration `{text}`: {e}"))
}

/// Least time each active run adds to the reserve: stopping it and storing its
/// final state.
const RESERVE_PER_RUN: Duration = Duration::from_millis(10);

/// Store writes a stopped run makes: its last events, its snapshot, its record.
const FINAL_WRITES_PER_RUN: u32 = 3;

/// Reserve one active run needs at the end of a drain, given the store's recent
/// write latency. Stores may serialise writes ([`crate::store::FileStore`] holds
/// one lock per write), so the runs' final writes are counted one after another;
/// at least [`RESERVE_PER_RUN`].
///
/// @param write_latency - Recent duration of one store write (`Duration`)
/// @returns Reserve per run (`Duration`)
pub fn reserve_per_run(write_latency: Duration) -> Duration {
    write_latency.saturating_mul(FINAL_WRITES_PER_RUN).max(RESERVE_PER_RUN)
}

/// When the drain stops waiting for in-flight nodes and preempts them: the end of
/// the timeout minus a reserve for cancelling and storing the runs (a quarter of
/// the timeout, between 0.5 s and 2 s, plus `per_run` per active run, never more
/// than the timeout itself).
///
/// @param timeout - The drain timeout (`Duration`)
/// @param runs - Active runs (`usize`)
/// @param per_run - Reserve per run, see [`reserve_per_run`] (`Duration`)
/// @returns Offset from the drain's start (`Duration`)
pub fn preempt_at(timeout: Duration, runs: usize, per_run: Duration) -> Duration {
    let per_runs = per_run.saturating_mul(u32::try_from(runs).unwrap_or(u32::MAX));
    let reserve = (timeout / 4).clamp(Duration::from_millis(500), Duration::from_secs(2)).saturating_add(per_runs);
    timeout.saturating_sub(reserve.min(timeout))
}

/// The pause reason a drain gives a run.
///
/// @param cause - What triggered the drain, e.g. `SIGTERM` (`&str`)
/// @param detached - Whether no caller waits for the run (`bool`)
/// @returns A reason of kind [`DRAINED`] (`PauseReason`)
pub fn drained_reason(cause: &str, detached: bool) -> PauseReason {
    PauseReason {
        frame: Arc::from(""),
        node_id: None,
        kind: DRAINED.to_owned(),
        data: Value::object([
            ("cause", Value::from(cause)),
            ("resume", Value::from(if detached { RESUME_AUTO } else { RESUME_CLIENT })),
        ]),
    }
}

/// Whether a stored run is one a drain paused and the server should resume by
/// itself: paused only by drains (no human input pending), with no caller
/// waiting for it.
pub fn is_auto_resumable(record: &RunRecord) -> bool {
    !record.pause_reasons.is_empty()
        && record.pause_reasons.iter().all(|reason| {
            reason.kind == DRAINED
                && reason.data.get_path(&["resume"]).as_ref().and_then(Value::as_str) == Some(RESUME_AUTO)
        })
}

/// Whether resuming `snapshot` needs secrets only its caller can supply (a
/// secret-typed start input, for instance): the snapshot holds them redacted.
pub fn needs_caller_secrets(snapshot: &Snapshot) -> bool {
    snapshot.to_json().map_or(true, |json| json.contains(&format!("\"{}\"", graphrt_core::value::SECRET_MARKER_KEY)))
}

/// SIGTERM and SIGINT (Ctrl-C), received for the lifetime of the server; on
/// Windows Ctrl+C, Ctrl+Break, closing the console and a system shutdown.
/// Plus the gateway's stop requests ([`ShutdownSignals::with_stop_requests`]).
///
/// Created before the server listens: from then on the signals are handled
/// (rather than ignored when PID 1, or fatal otherwise).
#[derive(Debug)]
pub struct ShutdownSignals {
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(windows)]
    windows: WindowsSignals,
    stop: Option<std::sync::Arc<tokio::sync::Notify>>,
}

/// The console events Windows delivers instead of signals.
#[cfg(windows)]
#[derive(Debug)]
struct WindowsSignals {
    interrupt: tokio::signal::windows::CtrlC,
    brk: tokio::signal::windows::CtrlBreak,
    close: tokio::signal::windows::CtrlClose,
    shutdown: tokio::signal::windows::CtrlShutdown,
}

impl ShutdownSignals {
    /// Install the handlers.
    ///
    /// @throws CliError - When the signal handlers cannot be installed
    pub fn install() -> Result<Self, CliError> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let install = |kind: SignalKind, name: &str| {
                signal(kind).map_err(|e| CliError::failed(format!("cannot handle {name}: {e}")))
            };
            Ok(Self {
                terminate: install(SignalKind::terminate(), "SIGTERM")?,
                interrupt: install(SignalKind::interrupt(), "SIGINT")?,
                stop: None,
            })
        }
        #[cfg(windows)]
        {
            use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_shutdown};
            let failed = |name: &str, e: std::io::Error| CliError::failed(format!("cannot handle {name}: {e}"));
            Ok(Self {
                windows: WindowsSignals {
                    interrupt: ctrl_c().map_err(|e| failed("Ctrl-C", e))?,
                    brk: ctrl_break().map_err(|e| failed("Ctrl-Break", e))?,
                    close: ctrl_close().map_err(|e| failed("console close", e))?,
                    shutdown: ctrl_shutdown().map_err(|e| failed("system shutdown", e))?,
                },
                stop: None,
            })
        }
        #[cfg(not(any(unix, windows)))]
        Ok(Self { stop: None })
    }

    /// Also stop on `stop`'s notifications (the gateway's `POST /internal/v1/stop`).
    #[must_use]
    pub fn with_stop_requests(mut self, stop: std::sync::Arc<tokio::sync::Notify>) -> Self {
        self.stop = Some(stop);
        self
    }

    /// Wait for the next signal and return its name.
    pub async fn recv(&mut self) -> &'static str {
        let stop = self.stop.clone();
        let requested = async move {
            match stop {
                Some(stop) => stop.notified().await,
                None => std::future::pending().await,
            }
        };
        let signal = std::pin::pin!(self.next_signal());
        match futures::future::select(signal, std::pin::pin!(requested)).await {
            Either::Left((name, _)) => name,
            Either::Right(((), _)) => "gateway stop",
        }
    }

    async fn next_signal(&mut self) -> &'static str {
        #[cfg(unix)]
        {
            let terminate = std::pin::pin!(self.terminate.recv());
            let interrupt = std::pin::pin!(self.interrupt.recv());
            match futures::future::select(terminate, interrupt).await {
                Either::Left(_) => "SIGTERM",
                Either::Right(_) => "SIGINT",
            }
        }
        #[cfg(windows)]
        {
            let signals = &mut self.windows;
            tokio::select! {
                _ = signals.interrupt.recv() => "Ctrl-C",
                _ = signals.brk.recv() => "Ctrl-Break",
                _ = signals.close.recv() => "console close",
                _ = signals.shutdown.recv() => "system shutdown",
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = tokio::signal::ctrl_c().await;
            "Ctrl-C"
        }
    }
}

/// How a drain ended.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DrainReport {
    /// Runs that were active during the drain.
    pub runs: usize,
    pub paused: usize,
    pub completed: usize,
    /// Runs that failed or were aborted while draining (by their own nodes).
    pub failed: usize,
    /// Runs still executing at the deadline (their store records stay `running`).
    pub unfinished: usize,
    /// Whether in-flight nodes had to be cancelled.
    pub preempted: bool,
    pub elapsed: Duration,
}

impl std::fmt::Display for DrainReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "drained {} run(s) in {:.2} s: {} paused, {} completed, {} failed, {} unfinished{}",
            self.runs,
            self.elapsed.as_secs_f64(),
            self.paused,
            self.completed,
            self.failed,
            self.unfinished,
            if self.preempted { " (in-flight nodes preempted)" } else { "" }
        )
    }
}

/// Drain `state` within `timeout`. `signals` delivers further signals: one
/// preempts the remaining runs at once.
///
/// @param state - The server state (`&AppState`)
/// @param timeout - The whole drain's deadline, from now (`Duration`)
/// @param cause - What triggered the drain, e.g. `SIGTERM` (`&str`)
/// @param signals - Signals received during the drain (`Option<&mut ShutdownSignals>`)
/// @returns How the active runs ended (`DrainReport`)
pub async fn drain(
    state: &AppState,
    timeout: Duration,
    cause: &str,
    mut signals: Option<&mut ShutdownSignals>,
) -> DrainReport {
    let started = Instant::now();
    let deadline = started + timeout;
    state.begin_drain();
    // Recomputed every poll: runs that finish on their own leave less to store,
    // writes made meanwhile refine the latency estimate.
    let preempt_deadline_now =
        || started + preempt_at(timeout, state.active_runs(), reserve_per_run(state.store_write_estimate()));
    let mut preempt_deadline = preempt_deadline_now();
    let mut forced = false;
    let mut seen: Vec<Arc<crate::store::live::ActiveRun>> = Vec::new();
    let mut preempted = false;
    loop {
        // Runs admitted just before the drain began show up late; pause them too.
        for run in state.active_list() {
            if seen.iter().any(|known| Arc::ptr_eq(known, &run)) {
                continue;
            }
            run.handle().pause_with(drained_reason(cause, run.is_detached()));
            if preempted {
                run.handle().preempt_with(None);
            }
            seen.push(run);
        }
        let now = Instant::now();
        // A run leaves the active list just before it marks itself done:
        // wait for both, or a run that just finished is reported unfinished.
        let settled = state.active_runs() == 0 && seen.iter().all(|run| run.is_done());
        if settled || now >= deadline {
            break;
        }
        if !forced {
            preempt_deadline = preempt_deadline_now();
        }
        if !preempted && now >= preempt_deadline {
            preempted = true;
            for run in &seen {
                run.handle().preempt_with(None);
            }
            continue;
        }
        let tick = tokio::time::sleep(POLL.min(deadline - now));
        match signals.as_deref_mut() {
            Some(signals) => {
                let again = std::pin::pin!(signals.recv());
                if let Either::Right((name, _)) = futures::future::select(std::pin::pin!(tick), again).await {
                    eprintln!("received {name} again: preempting active runs now");
                    preempt_deadline = Instant::now();
                    forced = true;
                }
            }
            None => tick.await,
        }
    }
    report(&seen, preempted, started.elapsed())
}

fn report(seen: &[Arc<crate::store::live::ActiveRun>], preempted: bool, elapsed: Duration) -> DrainReport {
    let mut report = DrainReport { runs: seen.len(), preempted, elapsed, ..DrainReport::default() };
    for run in seen {
        if !run.is_done() {
            report.unfinished += 1;
            continue;
        }
        match run.terminal_kind() {
            Some("paused") => report.paused += 1,
            Some("succeeded") => report.completed += 1,
            _ => report.failed += 1,
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_with_and_without_units() {
        assert_eq!(parse_duration("1500ms"), Ok(Duration::from_millis(1500)));
        assert_eq!(parse_duration("4s"), Ok(Duration::from_secs(4)));
        assert_eq!(parse_duration("2m"), Ok(Duration::from_secs(120)));
        assert_eq!(parse_duration("2.5"), Ok(Duration::from_millis(2500)));
        assert_eq!(parse_duration("0"), Ok(Duration::ZERO));
        for bad in ["", "-1s", "abc", "1h", "NaN", "inf"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn preemption_leaves_a_reserve_before_the_deadline() {
        let fast = reserve_per_run(Duration::from_micros(50));
        assert_eq!(fast, RESERVE_PER_RUN, "a fast store costs the minimum");
        assert_eq!(preempt_at(Duration::from_secs(4), 0, fast), Duration::from_secs(3));
        assert_eq!(preempt_at(Duration::from_secs(4), 10, fast), Duration::from_millis(2900));
        assert_eq!(preempt_at(Duration::from_secs(25), 0, fast), Duration::from_secs(23));
        assert_eq!(preempt_at(Duration::from_secs(1), 0, fast), Duration::from_millis(500));
        assert_eq!(preempt_at(Duration::from_millis(300), 0, fast), Duration::ZERO);
        assert_eq!(preempt_at(Duration::ZERO, 5, fast), Duration::ZERO);
        assert_eq!(preempt_at(Duration::from_secs(2), 1_000, fast), Duration::ZERO);
    }

    #[test]
    fn a_slow_store_moves_preemption_earlier() {
        // 40 ms per write, 3 writes per run: 10 runs need 1.2 s after the preemption.
        let slow = reserve_per_run(Duration::from_millis(40));
        assert_eq!(slow, Duration::from_millis(120));
        assert_eq!(preempt_at(Duration::from_secs(3), 10, slow), Duration::from_millis(1050));
        assert_eq!(preempt_at(Duration::from_secs(3), 30, slow), Duration::ZERO, "never before the drain began");
    }

    #[test]
    fn only_detached_drained_runs_resume_by_themselves() {
        let mut record = RunRecord::running("r", "wf", "fp", 0);
        assert!(!is_auto_resumable(&record), "no reasons");
        record.pause_reasons = vec![drained_reason("SIGTERM", true)];
        assert!(is_auto_resumable(&record));
        record.pause_reasons = vec![drained_reason("SIGTERM", false)];
        assert!(!is_auto_resumable(&record), "a caller resumes it");
        let human = PauseReason {
            frame: Arc::from(""),
            node_id: Some("ask".into()),
            kind: "human_input".into(),
            data: Value::Null,
        };
        record.pause_reasons = vec![drained_reason("SIGTERM", true), human];
        assert!(!is_auto_resumable(&record), "waits for a human");
    }
}
