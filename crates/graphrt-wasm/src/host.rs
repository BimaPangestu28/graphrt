//! Guest execution on the wasmi interpreter.
//!
//! A module is validated and compiled once. Every call gets a fresh `Store`
//! and instance, so guests cannot keep state between runs and one run cannot
//! observe another.

use std::fmt;
use std::sync::Arc;

use graphrt_core::node::NodeContext;
use graphrt_core::value::Value;

use wasmi::{
    Caller, Config, EnforcedLimits, Engine, Extern, Linker, Module, Store, StoreLimits, StoreLimitsBuilder, TrapCode,
};

use crate::abi::{ABI_VERSION, HOST_MODULE, pack, unpack};
use crate::error::WasmError;
use crate::limits::WasmLimits;
use crate::net::{self, HttpEgress, HttpError};

/// Imports every module may use.
const BASE_IMPORTS: &[&str] = &["log", "chunk", "progress"];
/// Import of plugins granted network access.
const HTTP_IMPORT: &str = "http";

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
    egress: Option<Arc<dyn HttpEgress>>,
    http_requests: usize,
    http_limits: WasmLimits,
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
    /// `graphrt_tool_call` with a tool request.
    Tool(&'a [u8]),
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
    imports_http: bool,
}

impl fmt::Debug for Compiled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Compiled").field("limits", &self.limits).finish_non_exhaustive()
    }
}

impl Compiled {
    /// Validate and compile `bytes`. `network` allows the `graphrt::http` import.
    pub(crate) fn new(bytes: &[u8], limits: WasmLimits, network: bool) -> Result<Self, WasmError> {
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
        let mut imports_http = false;
        for import in module.imports() {
            let host = import.module() == HOST_MODULE;
            if host && import.name() == HTTP_IMPORT {
                if !network {
                    return Err(WasmError::InvalidModule(format!(
                        "module imports `{HOST_MODULE}::{HTTP_IMPORT}`, but the plugin has no network access \
                         (`permissions.network` in its manifest, on a host built with outbound HTTP)"
                    )));
                }
                imports_http = true;
                continue;
            }
            if !host || !BASE_IMPORTS.contains(&import.name()) {
                return Err(WasmError::InvalidModule(format!(
                    "unsupported import `{}::{}` (only `{HOST_MODULE}::log|chunk|progress|http` are provided)",
                    import.module(),
                    import.name()
                )));
            }
        }
        let linker = build_linker(&engine)?;
        Ok(Self { engine, module, linker, limits, imports_http })
    }

    /// Whether the module imports `graphrt::http`.
    pub(crate) fn imports_http(&self) -> bool {
        self.imports_http
    }

    /// Whether the module exports `name`.
    pub(crate) fn exports(&self, name: &str) -> bool {
        self.module.exports().any(|export| export.name() == name)
    }

    pub(crate) fn limits(&self) -> &WasmLimits {
        &self.limits
    }

    /// Instantiate a fresh copy of the module and call one entry point.
    pub(crate) fn call(&self, call: Call<'_>, ctx: Option<NodeContext>) -> Result<CallOutput, WasmError> {
        self.call_logged(call, ctx, None).map_err(|f| f.error)
    }

    /// Like [`Compiled::call`], but a failure keeps the log lines the guest wrote
    /// before it failed: those usually explain the failure.
    /// `egress` serves the guest's `graphrt::http` requests; without it they fail.
    pub(crate) fn call_logged(
        &self,
        call: Call<'_>,
        ctx: Option<NodeContext>,
        egress: Option<Arc<dyn HttpEgress>>,
    ) -> Result<CallOutput, CallFailure> {
        let mut store = self.new_store(ctx, egress).map_err(|error| CallFailure { error, logs: Vec::new() })?;
        match self.call_in(&mut store, call) {
            Ok(bytes) => {
                let state = store.into_data();
                Ok(CallOutput { bytes, logs: state.logs, dropped_logs: state.dropped_logs })
            }
            Err(error) => Err(CallFailure { error, logs: store.into_data().logs }),
        }
    }

    fn new_store(
        &self,
        ctx: Option<NodeContext>,
        egress: Option<Arc<dyn HttpEgress>>,
    ) -> Result<Store<HostState>, WasmError> {
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
            egress,
            http_requests: 0,
            http_limits: limits.clone(),
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
            Call::Run(input) | Call::Tool(input) => {
                let (export, signature) = match call {
                    Call::Tool(_) => ("graphrt_tool_call", "graphrt_tool_call: (i32, i32) -> i64"),
                    _ => ("graphrt_run", "graphrt_run: (i32, i32) -> i64"),
                };
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
                    .get_typed_func::<(i32, i32), i64>(&store, export)
                    .map_err(|_| WasmError::MissingExport(signature.into()))?
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

/// Copy `len` bytes at `ptr` out of the caller's memory (at most `message_bytes`).
fn read_guest(caller: &Caller<'_, HostState>, ptr: i32, len: i32, what: &'static str) -> Result<Vec<u8>, wasmi::Error> {
    read_guest_up_to(caller, (ptr, len), caller.data().message_bytes, what)
}

/// Copy the `(ptr, len)` buffer out of the caller's memory, refusing more than `limit` bytes.
fn read_guest_up_to(
    caller: &Caller<'_, HostState>,
    (ptr, len): (i32, i32),
    limit: usize,
    what: &'static str,
) -> Result<Vec<u8>, wasmi::Error> {
    let (ptr, len) = (as_u32(ptr), as_u32(len));
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

/// Room for the method, URL and headers next to a request body.
const HTTP_ENVELOPE_BYTES: usize = 64 * 1024;

/// Largest request JSON for a body of `body_bytes`: base64 (`body_base64`) takes 4 bytes per 3.
fn http_request_json_limit(body_bytes: usize) -> usize {
    body_bytes.div_ceil(3).saturating_mul(4).saturating_add(HTTP_ENVELOPE_BYTES)
}

/// Validate one `graphrt::http` request, send it through the egress and encode the reply.
fn send_http(state: &mut HostState, request: &[u8]) -> Vec<u8> {
    let Some(egress) = state.egress.clone() else {
        let refused = HttpError::new("egress_denied", "outbound HTTP is not available in this call");
        return net::encode_result(Err(refused), net::ResponseEncoding::Utf8);
    };
    let max = state.http_limits.max_http_requests;
    if state.http_requests >= max {
        let refused = HttpError::new("too_large", format!("more than {max} HTTP requests in one call"));
        return net::encode_result(Err(refused), net::ResponseEncoding::Utf8);
    }
    state.http_requests += 1;
    match net::parse_request(request, &state.http_limits) {
        Ok(call) => net::encode_result(egress.send(call.request), call.response_encoding),
        Err(error) => net::encode_result(Err(error), net::ResponseEncoding::Utf8),
    }
}

/// Copy `bytes` into a buffer the guest allocates with `graphrt_alloc`; returns it packed.
fn write_to_guest(caller: &mut Caller<'_, HostState>, bytes: &[u8]) -> Result<i64, wasmi::Error> {
    let len = u32::try_from(bytes.len()).map_err(|e| host_trap(WasmError::Internal(e.to_string())))?;
    let alloc = caller
        .get_export("graphrt_alloc")
        .and_then(Extern::into_func)
        .ok_or_else(|| host_trap(WasmError::MissingExport("graphrt_alloc: (i32) -> i32".into())))?
        .typed::<i32, i32>(&*caller)
        .map_err(|_| host_trap(WasmError::MissingExport("graphrt_alloc: (i32) -> i32".into())))?;
    let ptr = alloc.call(&mut *caller, i32::from_ne_bytes(len.to_ne_bytes()))?;
    let memory = caller
        .get_export("memory")
        .and_then(Extern::into_memory)
        .ok_or_else(|| host_trap(WasmError::MissingExport("memory".into())))?;
    let offset = usize::try_from(as_u32(ptr)).map_err(|e| host_trap(WasmError::Internal(e.to_string())))?;
    memory
        .write(&mut *caller, offset, bytes)
        .map_err(|_| host_trap(WasmError::OutOfBounds { ptr: as_u32(ptr), len }))?;
    Ok(pack(as_u32(ptr), len))
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
    linker
        .func_wrap(HOST_MODULE, HTTP_IMPORT, |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| {
            charge(&mut caller, len_of(len))?;
            let limit = http_request_json_limit(caller.data().http_limits.http_request_bytes);
            let request = read_guest_up_to(&caller, (ptr, len), limit, "http request")?;
            let encoded = send_http(caller.data_mut(), &request);
            charge(&mut caller, u64::try_from(encoded.len()).unwrap_or(u64::MAX))?;
            write_to_guest(&mut caller, &encoded)
        })
        .map_err(link_err)?;
    Ok(linker)
}
