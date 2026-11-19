//! Guest execution on the wasmi interpreter.
//!
//! A module is validated and compiled once. Every call gets a fresh `Store`
//! and instance, so guests cannot keep state between runs and one run cannot
//! observe another.

use std::fmt;

use graphrt_core::node::NodeContext;
use graphrt_core::value::Value;

use wasmi::{
    Caller, Config, EnforcedLimits, Engine, Extern, Linker, Module, Store, StoreLimits, StoreLimitsBuilder, TrapCode,
};

use crate::abi::{ABI_VERSION, HOST_MODULE, unpack};
use crate::error::WasmError;
use crate::limits::WasmLimits;

/// One log line written by a guest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
    pub level: u8,
    pub message: String,
}

/// Per-call host state stored in the wasmi `Store`.
pub(crate) struct HostState {
    limits: StoreLimits,
    ctx: Option<NodeContext>,
    message_bytes: usize,
    max_logs: usize,
    logs: Vec<LogLine>,
    dropped_logs: usize,
    max_events: usize,
    events: usize,
    max_stream_bytes: usize,
    stream_bytes: usize,
}

/// Fixed fuel charged for every host call, on top of one unit per byte copied.
/// Host work is otherwise invisible to the fuel meter, so without this a guest
/// could make the host copy unbounded data within its fuel budget.
pub(crate) const HOST_CALL_FUEL: u64 = 1_000;

/// Raised from host functions to abort the guest with a precise error.
#[derive(Debug)]
struct HostTrap(WasmError);

impl fmt::Display for HostTrap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl wasmi::errors::HostError for HostTrap {}

/// Which guest entry point to call.
pub(crate) enum Call<'a> {
    Describe,
    Run(&'a [u8]),
}

/// Output of a guest call.
pub(crate) struct CallOutput {
    pub bytes: Vec<u8>,
    pub logs: Vec<LogLine>,
    pub dropped_logs: usize,
}

/// A validated, compiled module plus its import linker.
pub(crate) struct Compiled {
    engine: Engine,
    module: Module,
    linker: Linker<HostState>,
    limits: WasmLimits,
}

impl fmt::Debug for Compiled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Compiled").field("limits", &self.limits).finish_non_exhaustive()
    }
}

impl Compiled {
    /// Validate and compile `bytes`.
    pub(crate) fn new(bytes: &[u8], limits: WasmLimits) -> Result<Self, WasmError> {
        if bytes.len() > limits.module_bytes {
            return Err(WasmError::ModuleTooLarge { size: bytes.len(), limit: limits.module_bytes });
        }
        let mut config = Config::default();
        config
            .consume_fuel(true)
            .enforced_limits(EnforcedLimits::strict())
            .set_max_recursion_depth(limits.max_recursion_depth);
        let engine = Engine::new(&config);
        let module = Module::new(&engine, bytes).map_err(|e| WasmError::InvalidModule(e.to_string()))?;
        for import in module.imports() {
            if import.module() != HOST_MODULE || !matches!(import.name(), "log" | "chunk" | "progress") {
                return Err(WasmError::InvalidModule(format!(
                    "unsupported import `{}::{}` (only `{HOST_MODULE}::log|chunk|progress` are provided)",
                    import.module(),
                    import.name()
                )));
            }
        }
        let linker = build_linker(&engine)?;
        Ok(Self { engine, module, linker, limits })
    }

    pub(crate) fn limits(&self) -> &WasmLimits {
        &self.limits
    }

    /// Instantiate a fresh copy of the module and call one entry point.
    pub(crate) fn call(&self, call: Call<'_>, ctx: Option<NodeContext>) -> Result<CallOutput, WasmError> {
        self.call_logged(call, ctx).map_err(|f| f.error)
    }

    /// Like [`Compiled::call`], but a failure keeps the log lines the guest wrote
    /// before it failed: those usually explain the failure.
    pub(crate) fn call_logged(&self, call: Call<'_>, ctx: Option<NodeContext>) -> Result<CallOutput, CallFailure> {
        let mut store = self.new_store(ctx).map_err(|error| CallFailure { error, logs: Vec::new() })?;
        match self.call_in(&mut store, call) {
            Ok(bytes) => {
                let state = store.into_data();
                Ok(CallOutput { bytes, logs: state.logs, dropped_logs: state.dropped_logs })
            }
            Err(error) => Err(CallFailure { error, logs: store.into_data().logs }),
        }
    }

    fn new_store(&self, ctx: Option<NodeContext>) -> Result<Store<HostState>, WasmError> {
        let limits = &self.limits;
        let state = HostState {
            limits: StoreLimitsBuilder::new()
                .memory_size(limits.memory_bytes)
                .memories(1)
                // Tables live outside linear memory; keep them small so they do not
                // add tens of MB per instance on top of `memory_bytes`.
                .tables(4)
                .table_elements(100_000)
                .instances(1)
                .trap_on_grow_failure(true)
                .build(),
            ctx,
            message_bytes: limits.message_bytes,
            max_logs: limits.max_logs,
            logs: Vec::new(),
            dropped_logs: 0,
            max_events: limits.max_events,
            events: 0,
            max_stream_bytes: limits.max_stream_bytes,
            stream_bytes: 0,
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|s| &mut s.limits);
        store.set_fuel(limits.fuel).map_err(|e| WasmError::Internal(e.to_string()))?;
        Ok(store)
    }

    /// Run one entry point in `store`, returning the response bytes.
    fn call_in(&self, store: &mut Store<HostState>, call: Call<'_>) -> Result<Vec<u8>, WasmError> {
        let limits = &self.limits;
        let mut store = store;
        let instance = self.linker.instantiate_and_start(&mut store, &self.module).map_err(|e| match map_trap(&e) {
            Some(err) => err,
            None => WasmError::Instantiate(e.to_string()),
        })?;

        let version = instance
            .get_typed_func::<(), i32>(&store, "graphrt_abi_version")
            .map_err(|_| WasmError::MissingExport("graphrt_abi_version: () -> i32".into()))?
            .call(&mut store, ())
            .map_err(|e| call_error(&e))?;
        if version != ABI_VERSION {
            return Err(WasmError::AbiVersion { found: version, supported: ABI_VERSION });
        }
        let memory = instance.get_memory(&store, "memory").ok_or_else(|| WasmError::MissingExport("memory".into()))?;

        let packed = match call {
            Call::Describe => instance
                .get_typed_func::<(), i64>(&store, "graphrt_describe")
                .map_err(|_| WasmError::MissingExport("graphrt_describe: () -> i64".into()))?
                .call(&mut store, ())
                .map_err(|e| call_error(&e))?,
            Call::Run(input) => {
                if input.len() > limits.input_bytes {
                    return Err(WasmError::TooLarge { what: "request", size: input.len(), limit: limits.input_bytes });
                }
                let len = i32::try_from(input.len()).map_err(|_| WasmError::TooLarge {
                    what: "request",
                    size: input.len(),
                    limit: limits.input_bytes,
                })?;
                let ptr = instance
                    .get_typed_func::<i32, i32>(&store, "graphrt_alloc")
                    .map_err(|_| WasmError::MissingExport("graphrt_alloc: (i32) -> i32".into()))?
                    .call(&mut store, len)
                    .map_err(|e| call_error(&e))?;
                let offset = usize::try_from(as_u32(ptr)).map_err(|e| WasmError::Internal(e.to_string()))?;
                memory
                    .write(&mut store, offset, input)
                    .map_err(|_| WasmError::OutOfBounds { ptr: as_u32(ptr), len: as_u32(len) })?;
                instance
                    .get_typed_func::<(i32, i32), i64>(&store, "graphrt_run")
                    .map_err(|_| WasmError::MissingExport("graphrt_run: (i32, i32) -> i64".into()))?
                    .call(&mut store, (ptr, len))
                    .map_err(|e| call_error(&e))?
            }
        };

        let (ptr, len) = unpack(packed);
        let len_usize = usize::try_from(len).map_err(|e| WasmError::Internal(e.to_string()))?;
        if len_usize > limits.output_bytes {
            return Err(WasmError::TooLarge { what: "response", size: len_usize, limit: limits.output_bytes });
        }
        let start = usize::try_from(ptr).map_err(|e| WasmError::Internal(e.to_string()))?;
        let bytes = start
            .checked_add(len_usize)
            .and_then(|end| memory.data(&store).get(start..end))
            .ok_or(WasmError::OutOfBounds { ptr, len })?
            .to_vec();
        Ok(bytes)
    }
}

/// A failed guest call with the log lines written before the failure.
pub(crate) struct CallFailure {
    pub error: WasmError,
    pub logs: Vec<LogLine>,
}

fn as_u32(v: i32) -> u32 {
    u32::from_ne_bytes(v.to_ne_bytes())
}

/// Convert a trap raised while calling the guest.
fn call_error(e: &wasmi::Error) -> WasmError {
    map_trap(e).unwrap_or_else(|| WasmError::Trap(e.to_string()))
}

fn map_trap(e: &wasmi::Error) -> Option<WasmError> {
    if let Some(HostTrap(inner)) = e.downcast_ref::<HostTrap>() {
        return Some(inner.clone());
    }
    match e.as_trap_code()? {
        TrapCode::OutOfFuel => Some(WasmError::FuelExhausted),
        TrapCode::GrowthOperationLimited => Some(WasmError::MemoryLimit),
        other => Some(WasmError::Trap(other.to_string())),
    }
}

fn host_trap(err: WasmError) -> wasmi::Error {
    wasmi::Error::host(HostTrap(err))
}

/// Copy `len` bytes at `ptr` out of the caller's memory.
fn read_guest(caller: &Caller<'_, HostState>, ptr: i32, len: i32, what: &'static str) -> Result<Vec<u8>, wasmi::Error> {
    let (ptr, len) = (as_u32(ptr), as_u32(len));
    let limit = caller.data().message_bytes;
    let size = usize::try_from(len).map_err(|e| host_trap(WasmError::Internal(e.to_string())))?;
    if size > limit {
        return Err(host_trap(WasmError::TooLarge { what, size, limit }));
    }
    let memory = caller
        .get_export("memory")
        .and_then(Extern::into_memory)
        .ok_or_else(|| host_trap(WasmError::MissingExport("memory".into())))?;
    let start = usize::try_from(ptr).map_err(|e| host_trap(WasmError::Internal(e.to_string())))?;
    start
        .checked_add(size)
        .and_then(|end| memory.data(caller).get(start..end))
        .map(<[u8]>::to_vec)
        .ok_or_else(|| host_trap(WasmError::OutOfBounds { ptr, len }))
}

fn read_text(caller: &Caller<'_, HostState>, ptr: i32, len: i32, what: &'static str) -> Result<String, wasmi::Error> {
    let bytes = read_guest(caller, ptr, len, what)?;
    String::from_utf8(bytes).map_err(|_| host_trap(WasmError::InvalidOutput(format!("{what} is not valid UTF-8"))))
}

/// Charge the guest for host work: a fixed cost plus one unit per byte.
fn charge(caller: &mut Caller<'_, HostState>, bytes: u64) -> Result<(), wasmi::Error> {
    let cost = HOST_CALL_FUEL.saturating_add(bytes);
    let fuel = caller.get_fuel().map_err(|e| host_trap(WasmError::Internal(e.to_string())))?;
    if fuel < cost {
        return Err(host_trap(WasmError::FuelExhausted));
    }
    caller.set_fuel(fuel - cost).map_err(|e| host_trap(WasmError::Internal(e.to_string())))
}

/// Reserve one streamed event of `bytes` bytes against the per-call caps.
fn reserve_event(caller: &mut Caller<'_, HostState>, bytes: u64) -> Result<(), wasmi::Error> {
    let state = caller.data_mut();
    if state.events >= state.max_events {
        return Err(host_trap(WasmError::TooLarge {
            what: "streamed events",
            size: state.events.saturating_add(1),
            limit: state.max_events,
        }));
    }
    let total = state.stream_bytes.saturating_add(usize::try_from(bytes).unwrap_or(usize::MAX));
    if total > state.max_stream_bytes {
        return Err(host_trap(WasmError::TooLarge {
            what: "streamed bytes",
            size: total,
            limit: state.max_stream_bytes,
        }));
    }
    state.events += 1;
    state.stream_bytes = total;
    Ok(())
}

fn len_of(len: i32) -> u64 {
    u64::from(as_u32(len))
}

fn build_linker(engine: &Engine) -> Result<Linker<HostState>, WasmError> {
    let link_err = |e: wasmi::errors::LinkerError| WasmError::Internal(e.to_string());
    let mut linker = Linker::<HostState>::new(engine);
    linker
        .func_wrap(HOST_MODULE, "log", |mut caller: Caller<'_, HostState>, level: i32, ptr: i32, len: i32| {
            // Decide before copying anything, so dropped lines stay cheap.
            let full = {
                let state = caller.data();
                state.logs.len() >= state.max_logs
            };
            if full {
                charge(&mut caller, 0)?;
                let state = caller.data_mut();
                state.dropped_logs = state.dropped_logs.saturating_add(1);
                return Ok(());
            }
            charge(&mut caller, len_of(len))?;
            let message = read_text(&caller, ptr, len, "log line")?;
            let level = u8::try_from(level.clamp(0, 4)).unwrap_or(2);
            caller.data_mut().logs.push(LogLine { level, message });
            Ok(())
        })
        .map_err(link_err)?;
    linker
        .func_wrap(
            HOST_MODULE,
            "chunk",
            |mut caller: Caller<'_, HostState>, field_ptr: i32, field_len: i32, ptr: i32, len: i32| {
                let bytes = len_of(field_len).saturating_add(len_of(len));
                charge(&mut caller, bytes)?;
                reserve_event(&mut caller, bytes)?;
                let field = read_text(&caller, field_ptr, field_len, "chunk field")?;
                let text = read_text(&caller, ptr, len, "chunk")?;
                if let Some(ctx) = &caller.data().ctx {
                    ctx.chunk(&field, text);
                }
                Ok(())
            },
        )
        .map_err(link_err)?;
    linker
        .func_wrap(
            HOST_MODULE,
            "progress",
            |mut caller: Caller<'_, HostState>, name_ptr: i32, name_len: i32, ptr: i32, len: i32| {
                let bytes = len_of(name_len).saturating_add(len_of(len));
                charge(&mut caller, bytes)?;
                reserve_event(&mut caller, bytes)?;
                let name = read_text(&caller, name_ptr, name_len, "progress name")?;
                let payload = read_text(&caller, ptr, len, "progress payload")?;
                let data = match serde_json::from_str::<serde_json::Value>(&payload) {
                    Ok(json) => Value::from_json(json),
                    Err(_) => Value::from(payload),
                };
                if let Some(ctx) = &caller.data().ctx {
                    ctx.progress(&name, data);
                }
                Ok(())
            },
        )
        .map_err(link_err)?;
    Ok(linker)
}
