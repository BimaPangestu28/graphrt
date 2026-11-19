//! Python code via a local `python3` subprocess (feature `python`).
//!
//! **This is not a security sandbox.** The interpreter runs with the host
//! user's privileges and can read files, open sockets and spawn processes. Use
//! it only for trusted workflows (your own code, local development). For code
//! written by untrusted users, register a remote sandbox service instead.
//!
//! What it does provide is hygiene and resource bounds:
//!
//! - isolated interpreter (`-I -S`: no user site-packages, no `PYTHON*` env vars),
//! - empty environment except `PATH` (so secrets in the host env are not inherited),
//! - a fresh temporary working directory, removed afterwards,
//! - wall-clock timeout (the process is killed),
//! - on Unix, `ulimit` caps for CPU seconds, virtual memory and file size,
//! - capped stdout/stderr reads.
//!
//! User code defines `main(**inputs)` returning a `dict`. Anything the code
//! prints goes to a captured buffer and does not corrupt the result.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::future::BoxFuture;
use graphrt_core::node::NodeError;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::{CodeLimits, RemoteSandbox, SandboxRun};

/// Marker preceding the JSON result line on stdout.
const RESULT_MARKER: &str = "\u{1e}GRAPHRT_RESULT\u{1e}";

/// Bootstrap executed by the interpreter. Reads `{code, inputs}` from stdin.
const BOOTSTRAP: &str = r#"
import io, json, sys, traceback
_payload = json.loads(sys.stdin.read())
_real_stdout = sys.stdout
_captured = io.StringIO()
sys.stdout = _captured
_marker = "\x1eGRAPHRT_RESULT\x1e"
def _limits():
    try:
        import resource
        _inf = resource.RLIM_INFINITY
        _cpu = resource.getrlimit(resource.RLIMIT_CPU)[0]
        _mem = resource.getrlimit(resource.RLIMIT_AS)[0]
        return {"cpu": -1 if _cpu == _inf else _cpu, "memory": -1 if _mem == _inf else _mem}
    except Exception:
        return None
def _emit(obj):
    obj["stdout"] = _captured.getvalue()[-65536:]
    obj["limits"] = _limits()
    _real_stdout.write(_marker + json.dumps(obj, allow_nan=False) + "\n")
    _real_stdout.flush()
try:
    _ns = {"__name__": "__graphrt__"}
    exec(compile(_payload["code"], "<code>", "exec"), _ns)
    _main = _ns.get("main")
    if not callable(_main):
        _emit({"error": "code must define a function `main`", "kind": "invalid_code"})
        sys.exit(2)
    _result = _main(**_payload["inputs"])
    if not isinstance(_result, dict):
        _emit({"error": "main() must return a dict, got " + type(_result).__name__, "kind": "invalid_output"})
        sys.exit(2)
    try:
        _emit({"result": _result})
    except (TypeError, ValueError) as _e:
        _emit({"error": "result is not JSON-serializable: " + str(_e), "kind": "invalid_output"})
        sys.exit(2)
except SystemExit:
    raise
except MemoryError:
    _emit({"error": "out of memory", "kind": "resource_limit"})
    sys.exit(3)
except BaseException as _e:
    _tb = traceback.format_exception(type(_e), _e, _e.__traceback__)
    _emit({"error": "".join(_tb[-3:]).strip(), "kind": "code_error"})
    sys.exit(1)
"#;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Runs `python3` code in a local subprocess. Register as `Arc<dyn RemoteSandbox>`.
#[derive(Clone, Debug)]
pub struct SubprocessPython {
    interpreter: PathBuf,
    path_env: String,
}

impl Default for SubprocessPython {
    fn default() -> Self {
        Self { interpreter: PathBuf::from("python3"), path_env: "/usr/local/bin:/usr/bin:/bin".to_owned() }
    }
}

impl SubprocessPython {
    pub fn new() -> Self {
        Self::default()
    }

    /// Interpreter to run (default `python3`, resolved through `PATH`).
    pub fn interpreter(mut self, path: impl Into<PathBuf>) -> Self {
        self.interpreter = path.into();
        self
    }

    /// `PATH` given to the child (the rest of the environment is empty).
    pub fn path_env(mut self, path: impl Into<String>) -> Self {
        self.path_env = path.into();
        self
    }

    async fn run(&self, code: &str, inputs: serde_json::Value, limits: CodeLimits) -> Result<SandboxRun, NodeError> {
        let workdir = TempDir::create()?;
        let payload = serde_json::json!({ "code": code, "inputs": inputs });
        let payload = serde_json::to_vec(&payload)
            .map_err(|e| NodeError::fatal("invalid_input", format!("cannot encode inputs: {e}")))?;

        let mut command = self.command(&limits);
        command
            .current_dir(workdir.path())
            .env_clear()
            .env("PATH", &self.path_env)
            .env("PYTHONIOENCODING", "utf-8")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command
            .spawn()
            .map_err(|e| NodeError::fatal("sandbox", format!("cannot start {}: {e}", self.interpreter.display())))?;

        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let max = u64::try_from(limits.max_output_bytes).unwrap_or(u64::MAX).saturating_add(1024);

        let work = async {
            if let Some(mut stdin) = stdin {
                // A child that exits early closes stdin; that error surfaces via the exit status.
                let _ = stdin.write_all(&payload).await;
                drop(stdin);
            }
            let (out, err) = futures::join!(read_capped(stdout, max), read_capped(stderr, 64 * 1024));
            let status = child.wait().await;
            (out, err, status)
        };

        let timeout = Duration::from_millis(limits.time_ms.max(1));
        let (out, err, status) = match tokio::time::timeout(timeout, work).await {
            Ok(done) => done,
            Err(_) => {
                return Err(NodeError::fatal(
                    "timeout",
                    format!("python code exceeded the {} ms time limit", limits.time_ms),
                ));
            }
        };
        let status = status.map_err(|e| NodeError::new("sandbox", format!("waiting for python failed: {e}")))?;
        let (stdout_bytes, stdout_truncated) = out?;
        let (stderr_bytes, _) = err?;
        if stdout_truncated {
            return Err(NodeError::fatal(
                "invalid_output",
                format!("output exceeds the {}-byte limit", limits.max_output_bytes),
            ));
        }

        let stdout_text = String::from_utf8_lossy(&stdout_bytes);
        let result_line = stdout_text.lines().rev().find_map(|l| l.strip_prefix(RESULT_MARKER));
        let Some(line) = result_line else {
            let stderr_text = String::from_utf8_lossy(&stderr_bytes);
            let detail = stderr_text.trim();
            return Err(classify_crash(status.code(), detail, &limits));
        };
        let envelope: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| NodeError::fatal("sandbox", format!("malformed result from python: {e}")))?;
        let printed = envelope.get("stdout").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
        if let Some(error) = envelope.get("error").and_then(serde_json::Value::as_str) {
            let kind = envelope.get("kind").and_then(serde_json::Value::as_str).unwrap_or("code_error");
            // What the code printed before failing is usually the best clue.
            let message = if printed.trim().is_empty() {
                error.to_owned()
            } else {
                let tail: String = printed.chars().rev().take(2000).collect::<Vec<_>>().into_iter().rev().collect();
                format!("{error}\n--- printed output ---\n{}", tail.trim_end())
            };
            return Err(NodeError::fatal(kind, message));
        }
        let result = envelope
            .get("result")
            .cloned()
            .ok_or_else(|| NodeError::fatal("sandbox", "python result envelope has no `result`"))?;
        Ok(SandboxRun { result, stdout: Some(printed), notes: limit_notes(envelope.get("limits")) })
    }

    #[cfg(unix)]
    fn command(&self, limits: &CodeLimits) -> tokio::process::Command {
        // Resource caps are applied by the shell (`ulimit`) before `exec`-ing the interpreter,
        // which avoids `pre_exec` (unsafe). Unsupported limits are ignored silently by `2>/dev/null`.
        let cpu_seconds = limits.time_ms.div_ceil(1000).max(1).saturating_add(1);
        let memory_kib = u64::try_from(limits.memory_bytes / 1024).unwrap_or(u64::MAX).max(64 * 1024);
        // Interpreter startup needs some headroom beyond the user heap budget.
        let memory_kib = memory_kib.saturating_add(128 * 1024);
        let script = format!(
            "ulimit -t {cpu_seconds} 2>/dev/null; ulimit -v {memory_kib} 2>/dev/null; ulimit -f 20480 2>/dev/null; \
             ulimit -c 0 2>/dev/null; exec \"$0\" \"$@\""
        );
        let mut command = tokio::process::Command::new("/bin/sh");
        command.arg("-c").arg(script).arg(&self.interpreter).args(["-I", "-S", "-c", BOOTSTRAP]);
        command
    }

    #[cfg(not(unix))]
    fn command(&self, _limits: &CodeLimits) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.interpreter);
        command.args(["-I", "-S", "-c", BOOTSTRAP]);
        command
    }
}

impl RemoteSandbox for SubprocessPython {
    fn execute<'a>(
        &'a self,
        language: &'a str,
        code: &'a str,
        inputs: serde_json::Value,
        limits: CodeLimits,
    ) -> BoxFuture<'a, Result<serde_json::Value, NodeError>> {
        Box::pin(async move { self.execute_detailed(language, code, inputs, limits).await.map(|r| r.result) })
    }

    fn execute_detailed<'a>(
        &'a self,
        language: &'a str,
        code: &'a str,
        inputs: serde_json::Value,
        limits: CodeLimits,
    ) -> BoxFuture<'a, Result<SandboxRun, NodeError>> {
        Box::pin(async move {
            if language != "python3" {
                return Err(NodeError::fatal(
                    "unsupported",
                    format!("the python subprocess runner cannot execute `{language}` code"),
                ));
            }
            self.run(code, inputs, limits).await
        })
    }
}

/// Notes for resource limits that the platform did not apply (e.g. `ulimit -v` on macOS).
fn limit_notes(applied: Option<&serde_json::Value>) -> Vec<String> {
    if !cfg!(unix) {
        return vec!["resource limits are not applied on this platform; only the wall-clock timeout is".to_owned()];
    }
    let Some(applied) = applied.filter(|v| !v.is_null()) else {
        return vec!["could not verify the resource limits applied to python".to_owned()];
    };
    let mut notes = Vec::new();
    let unlimited = |key: &str| applied.get(key).and_then(serde_json::Value::as_i64) == Some(-1);
    if unlimited("cpu") {
        notes.push("CPU time limit was not applied; only the wall-clock timeout bounds this code".to_owned());
    }
    if unlimited("memory") {
        notes.push("memory limit was not applied (unsupported on this platform)".to_owned());
    }
    notes
}

fn classify_crash(code: Option<i32>, stderr: &str, limits: &CodeLimits) -> NodeError {
    let tail: String = stderr.chars().rev().take(2000).collect::<Vec<_>>().into_iter().rev().collect();
    match code {
        // Killed by a signal (e.g. SIGXCPU / SIGKILL from the CPU limit).
        None => NodeError::fatal(
            "resource_limit",
            format!("python was terminated (CPU limit {} ms or a signal). {tail}", limits.time_ms),
        ),
        Some(c) if tail.contains("MemoryError") || tail.contains("Cannot allocate memory") => {
            NodeError::fatal("resource_limit", format!("python ran out of memory (exit {c})"))
        }
        Some(c) => NodeError::fatal("code_error", format!("python exited with status {c} without a result: {tail}")),
    }
}

/// Read at most `max` bytes; the flag reports whether more data was available.
/// A read error is reported, never mistaken for complete output.
async fn read_capped<R: AsyncRead + Unpin>(reader: Option<R>, max: u64) -> Result<(Vec<u8>, bool), NodeError> {
    let Some(reader) = reader else {
        return Ok((Vec::new(), false));
    };
    let mut buf = Vec::new();
    let mut limited = reader.take(max.saturating_add(1));
    limited
        .read_to_end(&mut buf)
        .await
        .map_err(|e| NodeError::new("sandbox", format!("reading python output failed: {e}")))?;
    let truncated = u64::try_from(buf.len()).unwrap_or(u64::MAX) > max;
    if truncated {
        // Drain the rest so the child is not blocked on a full pipe; ignore content.
        let mut rest = limited.into_inner();
        let mut sink = [0u8; 8192];
        while let Ok(n) = rest.read(&mut sink).await {
            if n == 0 {
                break;
            }
        }
    }
    Ok((buf, truncated))
}

/// Temporary working directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn create() -> Result<Self, NodeError> {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("graphrt-py-{}-{n}-{nanos}", std::process::id()));
        // A fresh directory only: never adopt one (or a symlink) another local user
        // created at the predictable name. Owner-only on Unix.
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder
            .create(&dir)
            .map_err(|e| NodeError::new("sandbox", format!("cannot create a working directory: {e}")))?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Best effort: a leftover empty temp directory is harmless.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(code: &str, inputs: serde_json::Value, limits: CodeLimits) -> Result<serde_json::Value, NodeError> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(SubprocessPython::new().execute("python3", code, inputs, limits))
    }

    fn quick() -> CodeLimits {
        CodeLimits { time_ms: 5_000, ..CodeLimits::default() }
    }

    #[test]
    fn printed_output_is_returned_and_attached_to_failures() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let ok = rt
            .block_on(SubprocessPython::new().execute_detailed(
                "python3",
                "def main():\n    print('debug line')\n    return {'x': 1}",
                serde_json::json!({}),
                quick(),
            ))
            .unwrap();
        assert_eq!(ok.result, serde_json::json!({"x": 1}));
        assert_eq!(ok.stdout.as_deref().map(str::trim), Some("debug line"));
        #[cfg(target_os = "linux")]
        assert!(ok.notes.is_empty(), "limits should apply on Linux: {:?}", ok.notes);

        let err = run(
            "def main():\n    print('step 1 done')\n    raise RuntimeError('nope')",
            serde_json::json!({}),
            quick(),
        )
        .unwrap_err();
        assert!(err.message.contains("step 1 done"), "{}", err.message);
    }

    #[test]
    fn missing_limits_are_reported() {
        assert!(limit_notes(Some(&serde_json::json!({"cpu": 10, "memory": 1024}))).is_empty());
        let notes = limit_notes(Some(&serde_json::json!({"cpu": 10, "memory": -1})));
        assert!(notes.iter().any(|n| n.contains("memory")), "{notes:?}");
        assert!(!limit_notes(None).is_empty());
    }

    #[test]
    fn runs_main_with_kwargs() {
        let out = run(
            "def main(a, b):\n    print('noise')\n    return {'sum': a + b}",
            serde_json::json!({"a": 2, "b": 3}),
            quick(),
        )
        .unwrap();
        assert_eq!(out, serde_json::json!({"sum": 5}));
    }

    #[test]
    fn exceptions_are_reported() {
        let err = run("def main():\n    raise ValueError('bad thing')", serde_json::json!({}), quick()).unwrap_err();
        assert_eq!(err.kind, "code_error");
        assert!(err.message.contains("bad thing"), "{}", err.message);
        assert!(!err.retryable);
    }

    #[test]
    fn missing_main_and_bad_result() {
        let err = run("x = 1", serde_json::json!({}), quick()).unwrap_err();
        assert_eq!(err.kind, "invalid_code");
        let err = run("def main():\n    return 42", serde_json::json!({}), quick()).unwrap_err();
        assert_eq!(err.kind, "invalid_output");
        let err = run("def main():\n    return {'x': float('nan')}", serde_json::json!({}), quick()).unwrap_err();
        assert_eq!(err.kind, "invalid_output");
    }

    #[test]
    fn infinite_loop_times_out() {
        let limits = CodeLimits { time_ms: 500, ..CodeLimits::default() };
        let started = std::time::Instant::now();
        let err = run("def main():\n    while True:\n        pass", serde_json::json!({}), limits).unwrap_err();
        assert!(matches!(err.kind.as_str(), "timeout" | "resource_limit"), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn host_environment_is_not_inherited() {
        // SAFETY of the test: only reads the child's view of the environment.
        let out = run(
            "import os\ndef main():\n    return {'keys': sorted(os.environ.keys())}",
            serde_json::json!({}),
            quick(),
        )
        .unwrap();
        let keys: Vec<String> = serde_json::from_value(out["keys"].clone()).unwrap();
        // `__CF_USER_TEXT_ENCODING` is added by macOS CoreFoundation inside the
        // Python process itself (it appears even under `env -i`); it holds only
        // the uid and text encoding, nothing taken from the host environment.
        assert!(
            keys.iter().all(|k| matches!(
                k.as_str(),
                "PATH" | "PYTHONIOENCODING" | "LC_CTYPE" | "PWD" | "SHLVL" | "_" | "__CF_USER_TEXT_ENCODING"
            )),
            "{keys:?}"
        );
    }

    #[test]
    fn working_directory_is_fresh_and_private() {
        let dir = TempDir::create().unwrap();
        // Creating the same path again must fail rather than adopt the existing directory.
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        assert!(builder.create(dir.path()).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "working directory must be owner-only: {mode:o}");
        }
    }

    #[test]
    fn output_is_capped() {
        let limits = CodeLimits { max_output_bytes: 1024, ..quick() };
        let err = run("def main():\n    return {'x': 'a' * 100000}", serde_json::json!({}), limits).unwrap_err();
        assert_eq!(err.kind, "invalid_output");
    }

    #[test]
    fn code_node_runs_python_through_the_service() {
        use std::sync::Arc;

        use graphrt_core::dsl::NodeDoc;
        use graphrt_core::node::{Node, NodeContext, NodeOutcome, Services};
        use graphrt_core::{Value, VariablePool};

        let doc = NodeDoc::new(
            "py",
            "code",
            serde_json::json!({
                "code_language": "python3",
                "code": "def main(xs):\n    return {'total': sum(xs)}",
                "variables": [{"variable": "xs", "value_selector": ["start", "xs"]}],
                "outputs": {"total": {"type": "number"}}
            }),
        );
        let node = crate::CodeNode::from_doc(&doc).unwrap();
        let mut pool = VariablePool::new();
        pool.insert("start", "xs", Value::from_json(serde_json::json!([1, 2, 3])));
        let sandbox: Arc<dyn RemoteSandbox> = Arc::new(SubprocessPython::new());
        let ctx = NodeContext::for_test("py", pool).with_services(Arc::new(Services::new().with(sandbox)));
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = rt.block_on(node.run(ctx)).unwrap();
        let NodeOutcome::Completed(out) = outcome else { panic!("paused") };
        assert_eq!(out.outputs.get("total"), Some(&Value::Int(6)));
    }

    #[test]
    fn rejects_other_languages() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let err =
            rt.block_on(SubprocessPython::new().execute("ruby", "x", serde_json::json!({}), quick())).unwrap_err();
        assert_eq!(err.kind, "unsupported");
    }
}
