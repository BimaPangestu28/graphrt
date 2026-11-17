//! Child-process plugins: lazy start, crash detection and restart with backoff.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use graphrt_core::node::Clock;
use tokio::io::AsyncReadExt;

use crate::conn::{Connection, NotifySink};
use crate::discover::verify_checksums;
use crate::error::CallError;
use crate::manifest::{LoadedManifest, ProcessRuntime};
use crate::transport::{INITIALIZE_TIMEOUT, Transport, check_initialize, initialize_params, with_timeout};

/// Host variables always passed to plugin processes.
pub const DEFAULT_PASS_ENV: &[&str] = &["PATH", "HOME", "LANG", "TMPDIR", "SYSTEMROOT", "USERPROFILE", "APPDATA"];

/// How much plugin stderr is kept for diagnostics.
const STDERR_TAIL_BYTES: usize = 64 * 1024;

/// Upper bound of the restart backoff.
const MAX_BACKOFF: Duration = Duration::from_secs(5);

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
    next_id: AtomicU64,
    stderr: Arc<Mutex<VecDeque<u8>>>,
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
            next_id: AtomicU64::new(1),
            stderr: Arc::new(Mutex::new(VecDeque::new())),
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

    /// Return a live, initialized connection, starting the process if needed.
    async fn connection(&self, clock: &Arc<dyn Clock>) -> Result<(Arc<Connection>, serde_json::Value), CallError> {
        let mut live = self.live.lock().await;
        if let Some(l) = live.as_ref() {
            if !l.conn.is_closed() {
                return Ok((l.conn.clone(), l.init.clone()));
            }
            *live = None;
        }
        let failures = self.failures.load(Ordering::SeqCst);
        if failures > 0 && clock.supports_timers() {
            let exp = u32::try_from(failures.min(6)).unwrap_or(6);
            let backoff = Duration::from_millis(100u64.saturating_mul(1u64 << exp)).min(MAX_BACKOFF);
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

    async fn start(&self, clock: &Arc<dyn Clock>) -> Result<Live, CallError> {
        let name = &self.plugin.manifest.name;
        verify_checksums(&self.plugin).map_err(|e| CallError::Start(e.to_string()))?;
        // A relative command with a path separator (`./bin/plugin`) is relative to the
        // plugin directory; a bare name (`python3`) is looked up on PATH.
        let command = std::path::Path::new(&self.runtime.command);
        let program = if command.is_relative() && command.components().count() > 1 {
            self.plugin.dir.join(command)
        } else {
            command.to_path_buf()
        };
        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(&self.runtime.args);
        let cwd = match &self.runtime.cwd {
            Some(rel) => self.plugin.resolve(rel),
            None => self.plugin.dir.clone(),
        };
        cmd.current_dir(&cwd);
        if !self.runtime.inherit_env {
            cmd.env_clear();
            let names =
                DEFAULT_PASS_ENV.iter().copied().chain(self.plugin.manifest.permissions.env.iter().map(String::as_str));
            for var in names {
                if let Some(value) = std::env::var_os(var) {
                    cmd.env(var, value);
                }
            }
        }
        cmd.envs(&self.runtime.env);
        cmd.env("GRAPHRT_PLUGIN", name);
        cmd.env("GRAPHRT_PLUGIN_PROTOCOL", crate::manifest::PROTOCOL_VERSION.to_string());
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.kill_on_drop(true);

        let mut child =
            cmd.spawn().map_err(|e| CallError::Start(format!("cannot run `{}`: {e}", self.runtime.command)))?;
        let stdin = child.stdin.take().ok_or_else(|| CallError::Start("no stdin pipe".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| CallError::Start("no stdout pipe".into()))?;
        if let Some(mut stderr) = child.stderr.take() {
            if let Ok(mut tail) = self.stderr.lock() {
                tail.clear();
            }
            let sink = self.stderr.clone();
            tokio::spawn(async move {
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

        // The guard owns the child: dropping the connection kills the process.
        let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            tokio::select! {
                _ = child.wait() => {}
                _ = kill_rx => {
                    let _already_exited = child.start_kill();
                    let _status = child.wait().await;
                }
            }
        });
        let stderr_view = self.stderr.clone();
        let detail: crate::conn::CrashDetail = Arc::new(move || {
            // Give the stderr reader a moment to catch up is not possible synchronously;
            // report what has been captured so far.
            stderr_text(&stderr_view)
        });
        let conn = Arc::new(Connection::with_guard(
            stdout,
            stdin,
            self.plugin.manifest.limits.max_message_bytes,
            Some(Box::new(kill_tx)),
            Some(detail),
        ));

        let id = conn.next_id();
        let init =
            with_timeout(clock, INITIALIZE_TIMEOUT, conn.call(id, "initialize", initialize_params(), None, false))
                .await
                .map_err(|e| match e {
                    CallError::Crashed(m) => CallError::Start(m),
                    other => other,
                })?;
        check_initialize(&init)?;
        Ok(Live { conn, init })
    }
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
}
