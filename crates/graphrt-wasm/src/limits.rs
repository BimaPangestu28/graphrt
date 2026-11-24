//! Resource limits for guest modules.

use serde::{Deserialize, Serialize};

/// Limits applied to every guest call. Operators set these; workflow documents cannot raise them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WasmLimits {
    /// Fuel per call (roughly one unit per executed instruction). Bounds CPU
    /// time deterministically. Default 1 000 000 000.
    pub fuel: u64,
    /// Maximum linear memory per instance, in bytes. Default 64 MiB.
    pub memory_bytes: usize,
    /// Maximum size of the `.wasm` module, in bytes. Default 32 MiB.
    pub module_bytes: usize,
    /// Maximum request JSON size written into guest memory. Default 8 MiB.
    pub input_bytes: usize,
    /// Maximum response / manifest JSON size read from guest memory. Default 8 MiB.
    pub output_bytes: usize,
    /// Maximum size of one log line, chunk or progress payload. Default 256 KiB.
    pub message_bytes: usize,
    /// Maximum number of log lines kept per run (surfaced in node metadata). Default 100.
    pub max_logs: usize,
    /// Maximum number of `chunk` + `progress` events per call. Default 10 000.
    pub max_events: usize,
    /// Maximum total bytes streamed through `chunk` + `progress` per call. Default 16 MiB.
    pub max_stream_bytes: usize,
    /// Maximum wasm call-stack depth. Default 1024.
    pub max_recursion_depth: usize,
    /// Hand plaintext secrets to the guest when its config references them.
    /// Default `false`: secrets are masked. Enable only for plugins you trust.
    pub expose_secrets: bool,
    /// Maximum `graphrt::http` requests per call (plugins granted network access only). Default 16.
    pub max_http_requests: usize,
    /// Upper bound of one guest HTTP request's timeout, in ms; the guest may ask for less. Default 30 000.
    pub http_timeout_ms: u64,
    /// Maximum request body a guest may send, in bytes. Default 1 MiB.
    pub http_request_bytes: usize,
    /// Maximum response body handed back to a guest, in bytes. Default 4 MiB.
    pub http_response_bytes: usize,
}

impl Default for WasmLimits {
    fn default() -> Self {
        Self {
            fuel: 1_000_000_000,
            memory_bytes: 64 * 1024 * 1024,
            module_bytes: 32 * 1024 * 1024,
            input_bytes: 8 * 1024 * 1024,
            output_bytes: 8 * 1024 * 1024,
            message_bytes: 256 * 1024,
            max_logs: 100,
            max_events: 10_000,
            max_stream_bytes: 16 * 1024 * 1024,
            max_recursion_depth: 1024,
            expose_secrets: false,
            max_http_requests: 16,
            http_timeout_ms: 30_000,
            http_request_bytes: 1024 * 1024,
            http_response_bytes: 4 * 1024 * 1024,
        }
    }
}

impl WasmLimits {
    pub fn with_fuel(mut self, fuel: u64) -> Self {
        self.fuel = fuel;
        self
    }

    pub fn with_memory_bytes(mut self, bytes: usize) -> Self {
        self.memory_bytes = bytes;
        self
    }

    pub fn with_output_bytes(mut self, bytes: usize) -> Self {
        self.output_bytes = bytes;
        self
    }

    pub fn with_expose_secrets(mut self, expose: bool) -> Self {
        self.expose_secrets = expose;
        self
    }
}
