//! Errors.

use graphrt_core::node::NodeError;

/// Error loading or calling a guest module.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum WasmError {
    #[error("reading plugin `{path}`: {message}")]
    Io { path: String, message: String },
    #[error("module is {size} bytes, above the {limit}-byte limit")]
    ModuleTooLarge { size: usize, limit: usize },
    #[error("invalid wasm module: {0}")]
    InvalidModule(String),
    #[error("instantiating module failed: {0}")]
    Instantiate(String),
    #[error("module does not export `{0}` with the expected signature")]
    MissingExport(String),
    #[error("module implements ABI version {found}, host supports {supported}")]
    AbiVersion { found: i32, supported: i32 },
    #[error("invalid plugin manifest: {0}")]
    Manifest(String),
    #[error("node kind `{0}` is declared more than once")]
    DuplicateKind(String),
    #[error("plugin does not implement node kind `{0}`")]
    UnknownKind(String),
    #[error("node kind `{0}` is already registered")]
    KindConflict(String),
    #[error("guest ran out of fuel (CPU budget exhausted)")]
    FuelExhausted,
    #[error("guest exceeded its memory limit")]
    MemoryLimit,
    #[error("guest trapped: {0}")]
    Trap(String),
    #[error("{what} is {size} bytes, above the {limit}-byte limit")]
    TooLarge { what: &'static str, size: usize, limit: usize },
    #[error("guest returned an out-of-bounds buffer (ptr {ptr}, len {len})")]
    OutOfBounds { ptr: u32, len: u32 },
    #[error("guest returned invalid output: {0}")]
    InvalidOutput(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl WasmError {
    /// Short machine-readable category used as [`NodeError::kind`].
    pub fn kind(&self) -> &'static str {
        match self {
            WasmError::FuelExhausted => "wasm_fuel_exhausted",
            WasmError::MemoryLimit => "wasm_memory_limit",
            WasmError::Trap(_) => "wasm_trap",
            WasmError::TooLarge { .. } | WasmError::OutOfBounds { .. } | WasmError::InvalidOutput(_) => {
                "wasm_invalid_output"
            }
            WasmError::Internal(_) => "wasm_internal",
            _ => "wasm_plugin",
        }
    }
}

impl From<WasmError> for NodeError {
    /// Guest execution is deterministic, so every host-detected failure is fatal (not retried).
    fn from(e: WasmError) -> Self {
        NodeError::fatal(e.kind(), e.to_string())
    }
}
