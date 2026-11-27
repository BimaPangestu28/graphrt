//! Embedded JavaScript execution (QuickJS).
//!
//! Every execution gets a fresh runtime and context, so no state leaks between
//! runs. The context exposes only the ECMAScript standard library: no module
//! loader, file system, network or timers.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rquickjs::{Context, Runtime};

use graphrt_core::node::NodeError;

use crate::CodeLimits;

/// Global names used to exchange data with the script. Prefixed to avoid clashes
/// with user code.
const INPUT_GLOBAL: &str = "__graphrt_input";
const CODE_GLOBAL: &str = "__graphrt_code";

/// Driver script. It evaluates the user code with an indirect `eval` (so top-level
/// declarations become globals), then calls `main` through a promise chain, which
/// handles synchronous and `async` `main` uniformly. Every catchable error ends up
/// in `__graphrt_err` as text.
const DRIVER: &str = r#"
var __graphrt_out = undefined;
var __graphrt_err = undefined;
var __graphrt_done = false;
(function () {
  function describe(e) {
    try {
      if (e && typeof e === "object" && "message" in e) {
        var name = e.name ? e.name + ": " : "";
        return name + e.message;
      }
      return String(e);
    } catch (_) {
      return "unknown error";
    }
  }
  try {
    (0, eval)(__graphrt_code);
  } catch (e) {
    __graphrt_err = describe(e);
    __graphrt_done = true;
    return;
  }
  if (typeof main !== "function") {
    __graphrt_err = "code must define a function named `main`";
    __graphrt_done = true;
    return;
  }
  var args = JSON.parse(__graphrt_input);
  Promise.resolve()
    .then(function () { return main(args); })
    .then(
      function (value) {
        try {
          // NaN / Infinity would silently become null: reject them instead.
          __graphrt_out = JSON.stringify(value === undefined ? null : value, function (key, v) {
            if (typeof v === "number" && !isFinite(v)) {
              throw new Error("non-finite number (" + v + ") in result" + (key ? " at `" + key + "`" : ""));
            }
            return v;
          });
          if (__graphrt_out === undefined) {
            __graphrt_err = "main() returned a value that cannot be encoded as JSON";
          }
        } catch (e) {
          __graphrt_err = "cannot encode result: " + describe(e);
        }
        __graphrt_done = true;
      },
      function (e) {
        __graphrt_err = describe(e);
        __graphrt_done = true;
      }
    );
})();
"#;

/// Run `main(JSON.parse(input))` from `code` and return its JSON result.
pub(crate) fn run(code: &str, input: &str, limits: &CodeLimits) -> Result<serde_json::Value, NodeError> {
    let runtime = Runtime::new().map_err(|e| NodeError::new("sandbox", format!("cannot start JS runtime: {e}")))?;
    runtime.set_memory_limit(limits.memory_bytes);
    runtime.set_max_stack_size(limits.stack_bytes);

    let budget = Duration::from_millis(limits.time_ms);
    let deadline = Instant::now() + budget;
    // The interrupt handler records that it fired, so errors are classified by what the
    // engine actually did rather than by the clock at classification time.
    let interrupted = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&interrupted);
    runtime.set_interrupt_handler(Some(Box::new(move || {
        let stop = Instant::now() >= deadline;
        if stop {
            flag.store(true, Ordering::Relaxed);
        }
        stop
    })));

    let context =
        Context::full(&runtime).map_err(|e| NodeError::new("sandbox", format!("cannot create JS context: {e}")))?;

    let classify = |message: String| -> NodeError {
        if interrupted.load(Ordering::Relaxed) {
            // Running the same code again hits the same limit: not retryable.
            NodeError::fatal("timeout", format!("code exceeded its time limit of {} ms", budget.as_millis()))
        } else if is_engine_oom(&message) {
            NodeError::fatal(
                "out_of_memory",
                format!("code exceeded its memory limit of {} bytes", limits.memory_bytes),
            )
        } else {
            NodeError::fatal("code_error", message)
        }
    };

    // Phase 1: evaluate the driver (which evaluates the user code and schedules main).
    let setup: Result<(), String> = context.with(|ctx| {
        let globals = ctx.globals();
        globals.set(INPUT_GLOBAL, input).map_err(|e| e.to_string())?;
        globals.set(CODE_GLOBAL, code).map_err(|e| e.to_string())?;
        match ctx.eval::<(), _>(DRIVER) {
            Ok(()) => Ok(()),
            Err(rquickjs::Error::Exception) => Err(exception_text(&ctx)),
            Err(e) => Err(e.to_string()),
        }
    });
    setup.map_err(classify)?;

    // Phase 2: drive the promise chain to completion.
    loop {
        match runtime.execute_pending_job() {
            Ok(true) => continue,
            Ok(false) => break,
            Err(job_error) => {
                let message = job_error.0.with(|ctx| exception_text(&ctx));
                return Err(classify(message));
            }
        }
    }

    // Phase 3: read the outcome.
    let outcome: Result<(bool, Option<String>, Option<String>), String> = context.with(|ctx| {
        let globals = ctx.globals();
        let done: bool = globals.get("__graphrt_done").map_err(|e| e.to_string())?;
        let err: Option<String> = globals.get("__graphrt_err").map_err(|e| e.to_string())?;
        let out: Option<String> = globals.get("__graphrt_out").map_err(|e| e.to_string())?;
        Ok((done, err, out))
    });
    let (done, err, out) = outcome.map_err(classify)?;

    if let Some(message) = err {
        return Err(classify(message));
    }
    if !done {
        return Err(NodeError::fatal("code_error", "main() returned a promise that never settled"));
    }
    let text = out.unwrap_or_else(|| "null".to_owned());
    if text.len() > limits.max_output_bytes {
        return Err(NodeError::fatal(
            "invalid_output",
            format!("result is {} bytes, exceeding the limit of {} bytes", text.len(), limits.max_output_bytes),
        ));
    }
    serde_json::from_str(&text).map_err(|e| NodeError::fatal("invalid_output", format!("invalid result JSON: {e}")))
}

/// Whether a message is QuickJS's own out-of-memory error (not user text that merely
/// mentions memory).
fn is_engine_oom(message: &str) -> bool {
    let m = message.trim();
    m == "out of memory" || m == "InternalError: out of memory"
}

/// Extract a readable message from the pending exception.
fn exception_text(ctx: &rquickjs::Ctx<'_>) -> String {
    let value = ctx.catch();
    if let Some(obj) = value.as_object() {
        let message: Option<String> = obj.get("message").ok();
        if let Some(message) = message {
            return message;
        }
    }
    if let Some(s) = value.as_string()
        && let Ok(s) = s.to_string()
    {
        return s;
    }
    // Any other thrown value (number, object without `message`...): use its string form.
    if let Ok(rquickjs::convert::Coerced(text)) = value.get::<rquickjs::convert::Coerced<String>>() {
        return text;
    }
    "uncaught JavaScript exception".to_owned()
}
