//! Process control the gateway needs from the OS, without `unsafe` (the
//! workspace forbids it): signals through `/bin/kill` on Unix, `taskkill` on
//! Windows, and the stop requests the console delivers.

use std::process::Stdio;

/// Runs `program arguments` quietly; whether it ran and succeeded.
async fn quiet(program: &str, arguments: &[&str]) -> bool {
    tokio::process::Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

/// Sends `signal` (`TERM`, `KILL`) to the process group `pgid`.
#[cfg(unix)]
pub async fn signal_group(pgid: u32, signal: &str) -> bool {
    quiet("kill", &[&format!("-{signal}"), &format!("-{pgid}")]).await
}

/// Kills process `pid` at once (`SIGKILL`; `TerminateProcess` on Windows).
pub async fn kill(pid: u32) -> bool {
    if cfg!(windows) {
        quiet("taskkill", &["/F", "/PID", &pid.to_string()]).await
    } else {
        quiet("kill", &["-KILL", &pid.to_string()]).await
    }
}

/// Kills process `pid` and every process it started (`taskkill /T`).
#[cfg(windows)]
pub async fn kill_tree(pid: u32) -> bool {
    quiet("taskkill", &["/F", "/T", "/PID", &pid.to_string()]).await
}

/// Resolves when the process is asked to stop: `SIGTERM` or `SIGINT` on
/// Unix; Ctrl+C, Ctrl+Break, closing the console or a system shutdown on
/// Windows.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            () = async { match terminate.as_mut() { Some(signal) => { signal.recv().await; } None => std::future::pending().await } } => {}
        }
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_break, ctrl_close, ctrl_shutdown};
        // A handler that cannot be installed never fires; Ctrl+C still does.
        let mut breaks = ctrl_break().ok();
        let mut closes = ctrl_close().ok();
        let mut shutdowns = ctrl_shutdown().ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            () = async { match breaks.as_mut() { Some(signal) => { signal.recv().await; } None => std::future::pending().await } } => {}
            () = async { match closes.as_mut() { Some(signal) => { signal.recv().await; } None => std::future::pending().await } } => {}
            () = async { match shutdowns.as_mut() { Some(signal) => { signal.recv().await; } None => std::future::pending().await } } => {}
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
