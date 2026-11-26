//! Child-process plugins: lazy start, crash detection and restart with backoff.

use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use graphrt_core::child_env::{EnvFix, apply_env_fixes, host_env_fixes, set_child_env_with};
use graphrt_core::node::Clock;
use tokio::io::AsyncReadExt;

use crate::conn::{Connection, NotifySink};
use crate::discover::verify_checksums;
use crate::error::CallError;
use crate::manifest::{LoadedManifest, ProcessRuntime};
use crate::transport::{INITIALIZE_TIMEOUT, Transport, check_initialize, initialize_params, with_timeout};

/// Host variables always passed to plugin processes (shared with stdio MCP
/// servers; see [`graphrt_core::child_env`]).
pub use graphrt_core::child_env::DEFAULT_PASS_ENV;

/// How much plugin stderr is kept for diagnostics.
const STDERR_TAIL_BYTES: usize = 64 * 1024;

/// Upper bound of the restart backoff.
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// Consecutive request timeouts after which a plugin process is considered
/// wedged and is killed; the next request starts a fresh process.
pub const MAX_CONSECUTIVE_TIMEOUTS: u64 = 3;

struct Live {
    conn: Arc<Connection>,
    init: serde_json::Value,
}

/// Supervises one plugin process.
pub struct ProcessTransport {
    plugin: LoadedManifest,
    runtime: ProcessRuntime,
    live: tokio::sync::Mutex<Option<Live>>,
    /// Consecutive failed starts / crashes, used for backoff.
    failures: AtomicU64,
    /// Consecutive request timeouts of the current process.
    timeouts: AtomicU64,
    /// A kill was requested while the connection lock was busy; the next
    /// `connection()` call performs it before handing out the process.
    kill_pending: Mutex<Option<String>>,
    /// Fast-path flag mirroring `kill_pending.is_some()`.
    kill_requested: AtomicBool,
    next_id: AtomicU64,
    stderr: Arc<Mutex<VecDeque<u8>>>,
    /// How the last process exited (code or signal), for crash reports.
    exit_status: Arc<Mutex<Option<String>>>,
}

impl fmt::Debug for ProcessTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcessTransport")
            .field("plugin", &self.plugin.manifest.name)
            .field("command", &self.runtime.command)
            .finish_non_exhaustive()
    }
}

impl ProcessTransport {
    pub fn new(plugin: LoadedManifest, runtime: ProcessRuntime) -> Self {
        Self {
            plugin,
            runtime,
            live: tokio::sync::Mutex::new(None),
            failures: AtomicU64::new(0),
            timeouts: AtomicU64::new(0),
            kill_pending: Mutex::new(None),
            kill_requested: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            stderr: Arc::new(Mutex::new(VecDeque::new())),
            exit_status: Arc::new(Mutex::new(None)),
        }
    }

    /// Kill the current process (if any). The next request starts a new one.
    ///
    /// When the connection lock is busy the kill is recorded and performed by the
    /// next `connection()` call, so a wedged process is never silently kept.
    pub(crate) fn kill_current(&self, reason: &str) {
        match self.live.try_lock() {
            Ok(mut live) => {
                if let Some(l) = live.take() {
                    // Closing fails in-flight requests; once they release the connection
                    // its guard kills the process group.
                    l.conn.close(reason);
                }
            }
            Err(_) => {
                *self.kill_pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reason.to_owned());
                self.kill_requested.store(true, Ordering::SeqCst);
            }
        }
    }

    /// Stop the current process (if any) as soon as the connection lock is free.
    ///
    /// For a stop that nothing follows up on (an idle pool): a kill that
    /// [`Self::kill_current`] leaves pending only runs on the next request, so
    /// the process would stay up while the lock was busy for a moment, even
    /// just for an [`Self::is_ready`] check.
    pub(crate) async fn stop(&self, reason: &str) {
        let mut live = self.live.lock().await;
        self.kill_requested.store(false, Ordering::SeqCst);
        self.kill_pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        if let Some(l) = live.take() {
            l.conn.close(reason);
        }
    }

    /// The last bytes the plugin wrote to stderr.
    pub fn stderr_tail(&self) -> String {
        stderr_text(&self.stderr)
    }

    /// Recent `log` notifications of the running process.
    pub async fn recent_logs(&self) -> Vec<String> {
        self.live.lock().await.as_ref().map(|l| l.conn.recent_logs()).unwrap_or_default()
    }

    /// Whether a started, initialized process is up right now (`false` while
    /// one is starting).
    pub fn is_ready(&self) -> bool {
        self.live.try_lock().is_ok_and(|live| live.as_ref().is_some_and(|l| !l.conn.is_closed()))
    }

    /// The live connection, starting the process if needed (for the pool's keeper).
    pub(crate) async fn started(&self, clock: &Arc<dyn Clock>) -> Result<Arc<Connection>, CallError> {
        self.connection(clock).await.map(|(conn, _init)| conn)
    }

    /// Return a live, initialized connection, starting the process if needed.
    async fn connection(&self, clock: &Arc<dyn Clock>) -> Result<(Arc<Connection>, serde_json::Value), CallError> {
        let mut live = self.live.lock().await;
        if self.kill_requested.swap(false, Ordering::SeqCst) {
            let reason = self.kill_pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
            if let Some(l) = live.take() {
                l.conn.close(reason.as_deref().unwrap_or("plugin restart requested"));
            }
        }
        if let Some(l) = live.as_ref() {
            if !l.conn.is_closed() {
                return Ok((l.conn.clone(), l.init.clone()));
            }
            *live = None;
        }
        if let Some(backoff) = restart_backoff(self.failures.load(Ordering::SeqCst))
            && clock.supports_timers()
        {
            clock.sleep(backoff).await;
        }
        let started = self.start(clock).await;
        match started {
            Ok(l) => {
                let out = (l.conn.clone(), l.init.clone());
                *live = Some(l);
                Ok(out)
            }
            Err(e) => {
                self.failures.fetch_add(1, Ordering::SeqCst);
                Err(e)
            }
        }
    }

    /// Start the process and run the `initialize` handshake.
    async fn start(&self, clock: &Arc<dyn Clock>) -> Result<Live, CallError> {
        verify_checksums(&self.plugin).map_err(|e| CallError::Start(e.to_string()))?;
        let program = self.program()?;
        let mut child = self
            .command(&program)
            .spawn()
            .map_err(|e| CallError::Start(format!("cannot run `{}`: {e}", self.runtime.command)))?;
        let pid = child.id();
        if let Ok(mut status) = self.exit_status.lock() {
            *status = None;
        }
        let stdin = child.stdin.take().ok_or_else(|| CallError::Start("no stdin pipe".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| CallError::Start("no stdout pipe".into()))?;
        if let Some(stderr) = child.stderr.take() {
            self.collect_stderr(stderr);
        }
        // The guard owns the child: dropping the connection kills the process group.
        let kill_tx = supervise(child, pid, self.exit_status.clone());
        let max_message_bytes = self.plugin.manifest.limits.max_message_bytes;
        let conn = Arc::new(Connection::with_guard(
            stdout,
            stdin,
            max_message_bytes,
            Some(Box::new(kill_tx)),
            Some(self.crash_detail()),
        ));
        conn.set_label(&self.plugin.manifest.name);
        let init = initialize(&conn, clock, max_message_bytes).await?;
        Ok(Live { conn, init })
    }

    /// The program to run. A relative command with a path separator
    /// (`./bin/plugin`) is relative to the plugin directory; a bare name
    /// (`python3`) is looked up on PATH.
    fn program(&self) -> Result<PathBuf, CallError> {
        let command = Path::new(&self.runtime.command);
        // `has_root` too: on Windows `/usr/bin/x` is "relative" (no drive) but rooted.
        let in_plugin_dir = command.is_relative() && !command.has_root() && command.components().count() > 1;
        if !in_plugin_dir {
            return Ok(command.to_path_buf());
        }
        let program = self.plugin.dir.join(command);
        // A symlinked entrypoint could point anywhere, bypassing checksums.
        let meta = std::fs::symlink_metadata(&program)
            .map_err(|e| CallError::Start(format!("cannot stat `{}`: {e}", self.runtime.command)))?;
        if meta.file_type().is_symlink() {
            return Err(CallError::Start(format!(
                "`{}` is a symlink; plugin entrypoints must be files",
                self.runtime.command
            )));
        }
        Ok(program)
    }

    /// The configured command: arguments, working directory, environment,
    /// piped stdio and its own process group.
    fn command(&self, program: &Path) -> tokio::process::Command {
        self.command_with(program, host_env_fixes())
    }

    /// [`Self::command`] with explicit AppImage `fixes` (see `graphrt_core::child_env`).
    fn command_with(&self, program: &Path, fixes: &[EnvFix]) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(&self.runtime.args);
        let cwd = match &self.runtime.cwd {
            Some(rel) => self.plugin.resolve(rel),
            None => self.plugin.dir.clone(),
        };
        cmd.current_dir(&cwd);
        let pass_env = self.plugin.manifest.permissions.env.iter().map(String::as_str);
        set_child_env_with(cmd.as_std_mut(), self.runtime.inherit_env, pass_env, fixes);
        cmd.envs(&self.runtime.env);
        cmd.env("GRAPHRT_PLUGIN", &self.plugin.manifest.name);
        cmd.env("GRAPHRT_PLUGIN_PROTOCOL", crate::manifest::PROTOCOL_VERSION.to_string());
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.kill_on_drop(true);
        // Own process group, so wrappers (`sh run.sh`, `npx`, `uv run`) and every
        // process they start are killed together (on Windows `taskkill /T` walks the tree).
        #[cfg(unix)]
        cmd.process_group(0);
        cmd
    }

    /// Keep the last [`STDERR_TAIL_BYTES`] the new process writes to stderr,
    /// replacing the previous process's tail.
    fn collect_stderr(&self, mut stderr: tokio::process::ChildStderr) {
        if let Ok(mut tail) = self.stderr.lock() {
            tail.clear();
        }
        let sink = self.stderr.clone();
        graphrt_core::task::spawn(async move {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                if let Ok(mut tail) = sink.lock() {
                    tail.extend(buf.iter().take(n));
                    while tail.len() > STDERR_TAIL_BYTES {
                        tail.pop_front();
                    }
                }
            }
        });
    }

    /// What a crash report adds: how the process exited and its stderr tail.
    fn crash_detail(&self) -> crate::conn::CrashDetail {
        let stderr_view = self.stderr.clone();
        let status_view = self.exit_status.clone();
        Arc::new(move || {
            let status = status_view.lock().ok().and_then(|s| s.clone());
            let stderr = stderr_text(&stderr_view);
            match (status, stderr.is_empty()) {
                (Some(s), true) => s,
                (Some(s), false) => format!("{s}; stderr: {stderr}"),
                (None, false) => format!("stderr: {stderr}"),
                (None, true) => String::new(),
            }
        })
    }
}

/// Delay before the next start after `failures` consecutive failed starts or
/// crashes: none for the first start, then 200 ms doubling up to [`MAX_BACKOFF`].
pub(crate) fn restart_backoff(failures: u64) -> Option<Duration> {
    if failures == 0 {
        return None;
    }
    let exp = u32::try_from(failures.min(6)).unwrap_or(6);
    Some(Duration::from_millis(100u64.saturating_mul(1u64 << exp)).min(MAX_BACKOFF))
}

/// Wait for `child` in the background, killing its process tree when the
/// returned sender fires or is dropped, and record how it exited.
fn supervise(
    mut child: tokio::process::Child,
    pid: Option<u32>,
    exit_status: Arc<Mutex<Option<String>>>,
) -> tokio::sync::oneshot::Sender<()> {
    let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<()>();
    graphrt_core::task::spawn(async move {
        let status = tokio::select! {
            status = child.wait() => status,
            _ = kill_rx => {
                kill_tree(pid);
                let _already_exited = child.start_kill();
                child.wait().await
            }
        };
        // The main process is gone; reap anything it left in its group.
        kill_group(pid);
        if let Ok(mut slot) = exit_status.lock() {
            *slot = Some(match status {
                Ok(s) => describe_exit(&s),
                Err(e) => format!("unknown exit status ({e})"),
            });
        }
    });
    kill_tx
}

/// The `initialize` handshake. A process that dies during it failed to start.
async fn initialize(
    conn: &Connection,
    clock: &Arc<dyn Clock>,
    max_message_bytes: usize,
) -> Result<serde_json::Value, CallError> {
    let id = conn.next_id();
    let params = initialize_params(max_message_bytes);
    let init = with_timeout(clock, INITIALIZE_TIMEOUT, conn.call(id, "initialize", params, None, false))
        .await
        .map_err(|e| match e {
            CallError::Crashed(m) => CallError::Start(m),
            other => other,
        })?;
    check_initialize(&init)?;
    Ok(init)
}

/// Kill every process in the plugin's process group (Unix). Uses the `kill`
/// utility, so no unsafe libc call is needed. Safe after the plugin exited: the
/// group lives on while any member does.
fn kill_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        run_quietly(std::process::Command::new("kill").args(["-s", "KILL", "--", &format!("-{pid}")]));
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// Kill a still-running plugin and every process it started. Windows has no
/// process groups here, so `taskkill /T /F` walks the tree. Call it only while
/// the child is unreaped: its handle keeps the PID from being reused.
fn kill_tree(pid: Option<u32>) {
    #[cfg(windows)]
    if let Some(pid) = pid {
        run_quietly(std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]));
    }
    kill_group(pid);
}

#[cfg(any(unix, windows))]
/// Runs `command` (`kill`, `taskkill`) with the computer's own environment and
/// no output; its exit status does not matter.
fn run_quietly(command: &mut std::process::Command) {
    quiet_command(command, host_env_fixes());
    let _no_process_left = command.status();
}

fn quiet_command(command: &mut std::process::Command, fixes: &[EnvFix]) {
    apply_env_fixes(command, fixes);
    command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
}

fn describe_exit(status: &std::process::ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("process exited with code {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("process killed by signal {signal}");
        }
    }
    "process exited".to_owned()
}

fn stderr_text(buf: &Mutex<VecDeque<u8>>) -> String {
    let bytes: Vec<u8> = buf.lock().map(|b| b.iter().copied().collect()).unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let trimmed = text.trim();
    // Keep error messages readable: the last ~2 KiB are enough to diagnose a crash.
    let start = trimmed.len().saturating_sub(2048);
    let start = (start..trimmed.len()).find(|i| trimmed.is_char_boundary(*i)).unwrap_or(trimmed.len());
    trimmed.get(start..).unwrap_or_default().to_owned()
}

impl Transport for ProcessTransport {
    fn next_id(&self) -> String {
        format!("r-{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    fn call<'a>(
        &'a self,
        id: String,
        method: &'a str,
        params: serde_json::Value,
        sink: Option<NotifySink>,
        cancel_on_drop: bool,
        clock: &'a Arc<dyn Clock>,
    ) -> BoxFuture<'a, Result<serde_json::Value, CallError>> {
        Box::pin(async move {
            let (conn, _init) = self.connection(clock).await?;
            let result = conn.call(id, method, params, sink, cancel_on_drop).await;
            match &result {
                Err(CallError::Crashed(_)) => {
                    self.failures.fetch_add(1, Ordering::SeqCst);
                }
                _ => self.failures.store(0, Ordering::SeqCst),
            }
            result
        })
    }

    fn initialize<'a>(&'a self, clock: &'a Arc<dyn Clock>) -> BoxFuture<'a, Result<serde_json::Value, CallError>> {
        Box::pin(async move { self.connection(clock).await.map(|(_, init)| init) })
    }

    fn note_timeout(&self) {
        let count = self.timeouts.fetch_add(1, Ordering::SeqCst).saturating_add(1);
        if count >= MAX_CONSECUTIVE_TIMEOUTS {
            self.timeouts.store(0, Ordering::SeqCst);
            self.failures.fetch_add(1, Ordering::SeqCst);
            self.kill_current(&format!(
                "plugin `{}` stopped answering ({count} consecutive timeouts); restarting it",
                self.plugin.manifest.name
            ));
        }
    }

    fn note_answer(&self) {
        self.timeouts.store(0, Ordering::SeqCst);
    }

    fn is_warm(&self) -> bool {
        self.is_ready()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(command: &std::process::Command) -> Vec<(String, Option<String>)> {
        command
            .get_envs()
            .map(|(name, value)| {
                (name.to_string_lossy().into_owned(), value.map(|value| value.to_string_lossy().into_owned()))
            })
            .collect()
    }

    fn transport(inherit_env: bool) -> ProcessTransport {
        let manifest = crate::PluginManifest::from_yaml(&format!(
            "name: acme\nruntime: process\ncommand: node\nargs: [p.mjs]\ninherit_env: {inherit_env}\nenv: {{FOO: bar}}\n"
        ))
        .unwrap();
        let crate::manifest::Runtime::Process(runtime) = manifest.runtime.clone() else { panic!("process runtime") };
        ProcessTransport::new(LoadedManifest { manifest, dir: PathBuf::from(".") }, runtime)
    }

    /// What an AppImage's `AppRun` leaves for the plugin (see `graphrt_core::child_env`).
    fn appimage_fixes() -> Vec<EnvFix> {
        vec![EnvFix::Remove("LD_LIBRARY_PATH".into()), EnvFix::Set("PATH".into(), "/usr/bin".into())]
    }

    #[test]
    fn an_inheriting_plugin_gets_the_host_env_without_the_appimage() {
        let command = transport(true).command_with(Path::new("node"), &appimage_fixes());
        let env = env_of(command.as_std());
        assert!(env.contains(&("LD_LIBRARY_PATH".into(), None)), "{env:?}");
        assert!(env.contains(&("PATH".into(), Some("/usr/bin".into()))), "{env:?}");
        assert!(env.contains(&("FOO".into(), Some("bar".into()))), "the manifest's own env still applies");
    }

    #[test]
    fn a_minimal_plugin_gets_the_fixed_path() {
        let command = transport(false).command_with(Path::new("node"), &appimage_fixes());
        let env = env_of(command.as_std());
        assert!(env.contains(&("PATH".into(), Some("/usr/bin".into()))), "{env:?}");
        assert!(env.iter().all(|(name, _)| name != "LD_LIBRARY_PATH"), "{env:?}");
    }

    #[test]
    fn kill_runs_with_the_host_env_without_the_appimage() {
        let mut command = std::process::Command::new("kill");
        quiet_command(&mut command, &appimage_fixes());
        assert_eq!(env_of(&command), [("LD_LIBRARY_PATH".into(), None), ("PATH".into(), Some("/usr/bin".into()))]);
    }

    fn testkit() -> ProcessTransport {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join("testkit");
        let plugin = LoadedManifest::from_dir(&dir).unwrap();
        let crate::manifest::Runtime::Process(runtime) = plugin.manifest.runtime.clone() else {
            panic!("process runtime")
        };
        ProcessTransport::new(plugin, runtime)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stop_while_the_connection_lock_is_busy_still_stops_the_process() {
        let transport = Arc::new(testkit());
        let clock = graphrt_core::node::default_clock();
        transport.started(&clock).await.unwrap();
        assert!(transport.is_ready());

        // The lock is busy at the moment of the stop (an `is_ready` check, a request starting).
        let busy = transport.live.lock().await;
        let stopping = tokio::spawn({
            let transport = Arc::clone(&transport);
            async move { transport.stop("plugin pool idle").await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!stopping.is_finished(), "the stop waits for the lock");
        drop(busy);
        tokio::time::timeout(Duration::from_secs(10), stopping).await.unwrap().unwrap();
        assert!(!transport.is_ready(), "the process is stopped without another request");
    }

    #[test]
    fn restart_backoff_doubles_up_to_the_cap() {
        let delays: Vec<Option<u128>> =
            [0, 1, 2, 5, 6, 60].iter().map(|f| restart_backoff(*f).map(|d| d.as_millis())).collect();
        assert_eq!(delays, [None, Some(200), Some(400), Some(3200), Some(5000), Some(5000)]);
    }
}
