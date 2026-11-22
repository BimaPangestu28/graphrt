//! Opt-in anonymous usage telemetry of the `graphrt` binary.
//!
//! Off by default. It is on only when the user opted in (`graphrt telemetry
//! enable`, or `GRAPHRT_TELEMETRY=1`) and none of these holds:
//! `DO_NOT_TRACK` is set, a CI environment is detected, `GRAPHRT_TELEMETRY=0`,
//! or no endpoint is configured (`GRAPHRT_TELEMETRY_URL` at build time or at
//! run time). Without an endpoint nothing is counted, written or sent.
//!
//! While on, `run` and `serve` count runs and served requests in memory; the
//! counts are added to a small state file per UTC day ([`state`]) when the
//! process ends (hourly for `serve`). At most once a day a background thread
//! sends one [`report::Report`] with the complete days: it never delays a run,
//! and a report that cannot be sent in 5 seconds is dropped (its counts are
//! kept for the next day). A process that finishes while that report is in
//! flight waits for it (at most the send timeout) so it is not cut off.
//!
//! Only the binary ([`Session`] in `main`) persists or sends anything: library
//! users (and the FFI library, which does not link this crate) only ever touch
//! two in-memory counters.

pub mod report;
pub mod state;

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use crate::args::{Command, TelemetryArgs, TelemetryCommand};
use crate::error::{CliError, EXIT_OK};
use report::Report;
use state::{DayCounts, Paths, State};

/// Opt in (`1`, `true`, `yes`, `on`) or out (`0`, `false`, `no`, `off`) from the environment.
pub const OPT_IN_ENV: &str = "GRAPHRT_TELEMETRY";
/// Endpoint override at run time; the build-time value has the same name.
pub const URL_ENV: &str = "GRAPHRT_TELEMETRY_URL";
/// Endpoint compiled into this binary, if any.
const BUILD_URL: Option<&str> = option_env!("GRAPHRT_TELEMETRY_URL");
/// Variables whose presence means "running in CI".
const CI_VARS: &[&str] = &[
    "CI",
    "CONTINUOUS_INTEGRATION",
    "BUILD_NUMBER",
    "GITHUB_ACTIONS",
    "GITLAB_CI",
    "CIRCLECI",
    "TRAVIS",
    "BUILDKITE",
    "JENKINS_URL",
    "TEAMCITY_VERSION",
    "TF_BUILD",
    "BITBUCKET_BUILD_NUMBER",
    "CODEBUILD_BUILD_ID",
    "DRONE",
    "APPVEYOR",
    "SEMAPHORE",
    "BUILDKITE_BUILD_ID",
];
/// How often a long-running process (`serve`) saves counts and checks the daily report.
const BACKGROUND_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Slack on top of the send timeout when a finishing process waits for the report.
const EXIT_GRACE: Duration = Duration::from_millis(500);

static RUNS: AtomicU64 = AtomicU64::new(0);
static SERVE_REQUESTS: AtomicU64 = AtomicU64::new(0);

/// Count one started run (in memory only).
pub fn count_run() {
    RUNS.fetch_add(1, Ordering::Relaxed);
}

/// Count one served API request (in memory only).
pub fn count_serve_request() {
    SERVE_REQUESTS.fetch_add(1, Ordering::Relaxed);
}

/// Axum middleware counting served requests (`/healthz` is not routed through it).
#[cfg(feature = "serve")]
pub async fn count_requests(request: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    count_serve_request();
    next.run(request).await
}

/// Why telemetry is off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OffReason {
    DoNotTrack,
    Ci(&'static str),
    NoEndpoint,
    InvalidEndpoint(String),
    OptedOutByEnv,
    NotOptedIn,
    NoConfigDir,
}

impl std::fmt::Display for OffReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DoNotTrack => f.write_str("DO_NOT_TRACK is set"),
            Self::Ci(var) => write!(f, "running in CI (`{var}` is set)"),
            Self::NoEndpoint => f.write_str("no endpoint configured in this build (GRAPHRT_TELEMETRY_URL)"),
            Self::InvalidEndpoint(e) => f.write_str(e),
            Self::OptedOutByEnv => write!(f, "{OPT_IN_ENV} turns it off"),
            Self::NotOptedIn => write!(f, "not enabled; run `graphrt telemetry enable` or set {OPT_IN_ENV}=1"),
            Self::NoConfigDir => write!(f, "no user config directory (set {})", state::CONFIG_DIR_ENV),
        }
    }
}

/// The environment's say on telemetry, read once.
#[derive(Clone, Debug)]
pub struct Environment {
    do_not_track: bool,
    ci: Option<&'static str>,
    endpoint: Option<String>,
    opt_in: Option<bool>,
    paths: Option<Paths>,
}

/// `DO_NOT_TRACK` as <https://consoledonottrack.com> and docs/telemetry.md
/// define it: set to anything but `0` or `false`, including the empty string
/// (`DO_NOT_TRACK=` still says "do not track").
fn is_do_not_track(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        let value = value.trim();
        value != "0" && !value.eq_ignore_ascii_case("false")
    })
}

fn flag(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" => None,
        "0" | "false" | "no" | "off" => Some(false),
        _ => Some(true),
    }
}

impl Environment {
    /// Read the settings through `lookup` (an environment variable getter).
    pub fn from_lookup(lookup: &dyn Fn(&str) -> Option<String>) -> Self {
        let set = |name: &str| lookup(name).and_then(|v| flag(&v)).unwrap_or(false);
        Self {
            do_not_track: is_do_not_track(lookup("DO_NOT_TRACK").as_deref()),
            ci: CI_VARS.iter().copied().find(|name| set(name)),
            endpoint: lookup(URL_ENV).or_else(|| BUILD_URL.map(str::to_owned)).filter(|url| !url.trim().is_empty()),
            opt_in: lookup(OPT_IN_ENV).and_then(|v| flag(&v)),
            paths: Paths::locate(lookup),
        }
    }

    /// The process environment.
    pub fn current() -> Self {
        Self::from_lookup(&|name| std::env::var(name).ok())
    }

    /// Reasons that keep telemetry off whatever the user chose.
    pub fn blocked(&self) -> Option<OffReason> {
        if self.do_not_track {
            return Some(OffReason::DoNotTrack);
        }
        if let Some(var) = self.ci {
            return Some(OffReason::Ci(var));
        }
        match &self.endpoint {
            None => return Some(OffReason::NoEndpoint),
            Some(raw) => {
                if let Err(e) = report::parse_endpoint(raw) {
                    return Some(OffReason::InvalidEndpoint(e));
                }
            }
        }
        if self.opt_in == Some(false) {
            return Some(OffReason::OptedOutByEnv);
        }
        if self.paths.is_none() {
            return Some(OffReason::NoConfigDir);
        }
        None
    }

    /// Whether telemetry is on given the state file; `Err` says why not.
    pub fn decide(&self, state: &State) -> Result<(), OffReason> {
        if let Some(reason) = self.blocked() {
            return Err(reason);
        }
        if self.opt_in == Some(true) || state.enabled { Ok(()) } else { Err(OffReason::NotOptedIn) }
    }

    /// Whether telemetry is on, judged without touching the disk beyond
    /// reading the state file. A build with an endpoint that the user never
    /// opted into must not create the config directory or `telemetry.lock`.
    fn is_on(&self) -> bool {
        let Some(paths) = &self.paths else { return false };
        self.decide(&state::read(paths)).is_ok()
    }

    fn endpoint_url(&self) -> Option<reqwest::Url> {
        self.endpoint.as_deref().and_then(|raw| report::parse_endpoint(raw).ok())
    }
}

/// Whether a command's usage is counted and reported (`run` and `serve`).
pub fn tracks_usage(command: &Command) -> bool {
    match command {
        Command::Run(_) => true,
        #[cfg(feature = "serve")]
        Command::Serve(_) => true,
        _ => false,
    }
}

/// Telemetry for one process of the binary: started before the command, ended after it.
#[derive(Debug)]
pub struct Session {
    environment: Option<Environment>,
    /// Signalled when the background thread finished its first round (counts
    /// saved, daily report sent or not needed).
    first_round: Option<mpsc::Receiver<()>>,
}

impl Session {
    /// Start the background reporter when telemetry may be on. Cheap: only
    /// environment lookups happen on the calling thread.
    pub fn begin(command: &Command) -> Self {
        if !tracks_usage(command) {
            return Self { environment: None, first_round: None };
        }
        let environment = Environment::current();
        if environment.blocked().is_some() {
            return Self { environment: None, first_round: None };
        }
        let background = environment.clone();
        let (done, first_round) = mpsc::channel();
        let spawned = std::thread::Builder::new().name("graphrt-telemetry".into()).spawn(move || {
            run_background(&background, done);
        });
        // No thread, no report: telemetry must never get in the way.
        let first_round = spawned.is_ok().then_some(first_round);
        Self { environment: Some(environment), first_round }
    }

    /// Save this process's counts (no network).
    ///
    /// A short command such as `graphrt run` usually ends while the background
    /// thread is still sending the day's report; exiting then would kill the
    /// request after the day was already claimed, losing the report. So this
    /// first waits for the thread's first round, which sends at most once a day
    /// and gives up after [`report::SEND_TIMEOUT`]. On every other run the round
    /// is a state-file read and the wait is negligible.
    pub fn end(self) {
        if let Some(first_round) = &self.first_round {
            let _finished_or_timed_out = first_round.recv_timeout(report::SEND_TIMEOUT + EXIT_GRACE);
        }
        if let Some(environment) = &self.environment {
            save_counts(environment);
        }
    }
}

/// Save counts and send the daily report, then repeat hourly while the process lives.
fn run_background(environment: &Environment, first_round: mpsc::Sender<()>) {
    let mut first_round = Some(first_round);
    loop {
        save_counts(environment);
        let still_on = send_daily_report(environment);
        if let Some(done) = first_round.take() {
            // The process may already be gone; nobody listening is fine.
            let _ = done.send(());
        }
        if !still_on {
            return;
        }
        std::thread::sleep(BACKGROUND_INTERVAL);
    }
}

/// Move the in-memory counts into today's entry of the state file (when on).
fn save_counts(environment: &Environment) {
    let Some(paths) = &environment.paths else { return };
    if !environment.is_on() {
        return;
    }
    let counts =
        DayCounts { runs: RUNS.swap(0, Ordering::Relaxed), serve_requests: SERVE_REQUESTS.swap(0, Ordering::Relaxed) };
    if counts == DayCounts::default() {
        return;
    }
    let today = state::today();
    let saved = state::update(paths, |state| {
        if environment.decide(state).is_err() {
            return Ok(());
        }
        state.ensure_install_id()?;
        state.record(&today, counts);
        Ok::<(), String>(())
    });
    // Telemetry problems are never the user's problem: drop the counts silently.
    let _ignored = saved;
}

/// Send today's report if none was sent yet. Returns whether telemetry is still on.
fn send_daily_report(environment: &Environment) -> bool {
    let (Some(paths), Some(url)) = (&environment.paths, environment.endpoint_url()) else { return false };
    if !environment.is_on() {
        return false;
    }
    let today = state::today();
    let claimed = state::update(paths, |state| {
        if environment.decide(state).is_err() {
            return Ok(None);
        }
        if state.last_sent_day.as_deref() == Some(today.as_str()) {
            return Ok(Some(None));
        }
        let install_id = state.ensure_install_id()?;
        let days = state.take_completed(&today);
        // Claimed before sending: concurrent processes send once a day between them.
        state.last_sent_day = Some(today.clone());
        Ok::<_, String>(Some(Some((Report::new(install_id, &days), days))))
    });
    let (report, days) = match claimed {
        Ok(Ok(Some(Some(claim)))) => claim,
        Ok(Ok(Some(None))) => return true,
        _ => return false,
    };
    if report::send(&url, &report).is_err() {
        // Keep the counts for tomorrow's report.
        let _ignored = state::update(paths, |state| state.restore(days));
    }
    true
}

/// What `graphrt telemetry status` shows and `--json` prints.
fn status_json(environment: &Environment) -> serde_json::Value {
    let state = environment.paths.as_ref().map(state::read).unwrap_or_default();
    let decision = environment.decide(&state);
    let mut pending = state.clone();
    let days = pending.take_completed(&state::today());
    let install_id = state.install_id.clone().unwrap_or_else(|| report::PENDING_INSTALL_ID.to_owned());
    serde_json::json!({
        "enabled": decision.is_ok(),
        "reason": decision.err().map(|reason| reason.to_string()),
        "endpoint": environment.endpoint.as_deref().map(graphrt_core::urls::redact_userinfo),
        "state_file": environment.paths.as_ref().map(|p| p.file.display().to_string()),
        "last_sent_day": state.last_sent_day,
        "next_report": Report::new(install_id, &days),
    })
}

/// `graphrt telemetry enable|disable|status`.
pub fn telemetry(args: &TelemetryArgs) -> Result<u8, CliError> {
    let environment = Environment::current();
    let mut out = std::io::stdout().lock();
    match &args.command {
        TelemetryCommand::Enable => enable(&environment, &mut out)?,
        TelemetryCommand::Disable => disable(&environment, &mut out)?,
        TelemetryCommand::Status { json: true } => {
            let status = status_json(&environment);
            writeln!(out, "{}", serde_json::to_string_pretty(&status).unwrap_or_default()).map_err(output_error)?;
        }
        TelemetryCommand::Status { json: false } => print_status(&status_json(&environment), &mut out)?,
    }
    Ok(EXIT_OK)
}

fn output_error(e: std::io::Error) -> CliError {
    CliError::failed(e.to_string())
}

/// @throws CliError (usage) - Without a user config directory.
fn state_paths(environment: &Environment) -> Result<Paths, CliError> {
    environment
        .paths
        .clone()
        .ok_or_else(|| CliError::usage(format!("cannot find a user config directory; set {}", state::CONFIG_DIR_ENV)))
}

/// `graphrt telemetry enable`: opt in, creating the install id.
fn enable(environment: &Environment, out: &mut impl Write) -> Result<(), CliError> {
    let paths = state_paths(environment)?;
    state::update(&paths, |state| {
        state.enabled = true;
        state.ensure_install_id().map(|_| ())
    })
    .and_then(|created| created)
    .map_err(CliError::failed)?;
    writeln!(out, "telemetry enabled ({})", paths.file.display()).map_err(output_error)?;
    if let Some(reason) = environment.blocked() {
        writeln!(out, "note: nothing is sent while {reason}").map_err(output_error)?;
    }
    writeln!(out, "see `graphrt telemetry status` for exactly what is sent").map_err(output_error)
}

/// `graphrt telemetry disable`: opt out and forget the install id and unsent counts.
fn disable(environment: &Environment, out: &mut impl Write) -> Result<(), CliError> {
    let paths = state_paths(environment)?;
    state::update(&paths, |state| *state = State::default()).map_err(CliError::failed)?;
    writeln!(out, "telemetry disabled; install id and unsent counts deleted").map_err(output_error)
}

/// `graphrt telemetry status` as text.
fn print_status(status: &serde_json::Value, out: &mut impl Write) -> Result<(), CliError> {
    let text = |key: &str| status.get(key).and_then(serde_json::Value::as_str).map(str::to_owned);
    match text("reason") {
        None => writeln!(out, "telemetry: on").map_err(output_error)?,
        Some(reason) => writeln!(out, "telemetry: off ({reason})").map_err(output_error)?,
    }
    writeln!(out, "endpoint: {}", text("endpoint").unwrap_or_else(|| "none".into())).map_err(output_error)?;
    writeln!(out, "state file: {}", text("state_file").unwrap_or_else(|| "none".into())).map_err(output_error)?;
    writeln!(out, "last report: {}", text("last_sent_day").unwrap_or_else(|| "never".into())).map_err(output_error)?;
    writeln!(out, "next report (sent at most once a day, only while on):").map_err(output_error)?;
    let payload = serde_json::to_string_pretty(&status.get("next_report")).unwrap_or_default();
    writeln!(out, "{payload}").map_err(output_error)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn environment(vars: &[(&str, &str)]) -> Environment {
        let map: HashMap<String, String> = vars.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        Environment::from_lookup(&|name| map.get(name).cloned())
    }

    const BASE: [(&str, &str); 2] =
        [(URL_ENV, "https://telemetry.example.com/v1/report"), (state::CONFIG_DIR_ENV, "/tmp/graphrt-telemetry-unit")];

    fn with(extra: &[(&'static str, &'static str)]) -> Environment {
        let mut vars = BASE.to_vec();
        vars.extend_from_slice(extra);
        environment(&vars)
    }

    fn enabled() -> State {
        State { enabled: true, ..State::default() }
    }

    #[test]
    fn off_by_default() {
        assert_eq!(with(&[]).decide(&State::default()), Err(OffReason::NotOptedIn));
    }

    #[test]
    fn opt_in_by_command_or_env() {
        assert_eq!(with(&[]).decide(&enabled()), Ok(()));
        assert_eq!(with(&[(OPT_IN_ENV, "1")]).decide(&State::default()), Ok(()));
        assert_eq!(with(&[(OPT_IN_ENV, "0")]).decide(&enabled()), Err(OffReason::OptedOutByEnv));
    }

    #[test]
    fn do_not_track_ci_and_missing_endpoint_always_win() {
        let opted = [(OPT_IN_ENV, "1")];
        assert_eq!(with(&[opted[0], ("DO_NOT_TRACK", "1")]).decide(&enabled()), Err(OffReason::DoNotTrack));
        assert_eq!(with(&[opted[0], ("CI", "true")]).decide(&enabled()), Err(OffReason::Ci("CI")));
        assert_eq!(
            with(&[opted[0], ("GITHUB_ACTIONS", "true")]).decide(&enabled()),
            Err(OffReason::Ci("GITHUB_ACTIONS"))
        );
        // `CI=false` is not CI.
        assert_eq!(with(&[opted[0], ("CI", "false")]).decide(&enabled()), Ok(()));
        if BUILD_URL.is_none() {
            let no_url = environment(&[(OPT_IN_ENV, "1"), (state::CONFIG_DIR_ENV, "/tmp/x")]);
            assert_eq!(no_url.decide(&enabled()), Err(OffReason::NoEndpoint));
        }
        let plain_http = environment(&[(URL_ENV, "http://example.com/r"), (state::CONFIG_DIR_ENV, "/tmp/x")]);
        assert!(matches!(plain_http.decide(&enabled()), Err(OffReason::InvalidEndpoint(_))));
    }

    #[test]
    fn do_not_track_is_set_by_any_value_but_0_or_false() {
        for value in ["1", "true", "", " ", "yes", "no", "off"] {
            assert_eq!(with(&[("DO_NOT_TRACK", value)]).decide(&enabled()), Err(OffReason::DoNotTrack), "{value:?}");
        }
        for value in ["0", "false", "FALSE", " 0 "] {
            assert_eq!(with(&[("DO_NOT_TRACK", value)]).decide(&enabled()), Ok(()), "{value:?}");
        }
    }

    /// A build with an endpoint, run without opting in: counting and the
    /// background round leave no config directory or lock file behind.
    #[test]
    fn nothing_touches_disk_unless_telemetry_is_on() {
        let dir = std::env::temp_dir().join(format!("graphrt-telemetry-no-disk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config_dir = dir.to_string_lossy().into_owned();
        let map: HashMap<String, String> = [
            (URL_ENV.to_owned(), "https://telemetry.example.com/v1/report".to_owned()),
            (state::CONFIG_DIR_ENV.to_owned(), config_dir),
        ]
        .into();
        let environment = Environment::from_lookup(&|name| map.get(name).cloned());
        assert!(!environment.is_on());
        count_run();
        save_counts(&environment);
        assert!(!send_daily_report(&environment));
        assert!(!dir.exists(), "telemetry off must not create {}", dir.display());
    }
}
