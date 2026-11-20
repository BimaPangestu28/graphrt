//! # graphrt-eval
//!
//! Evaluate graphrt workflows against test cases, in CI or from `cargo test`.
//!
//! - [`EvalSuite`]: an eval file (`*.eval.yaml`, Promptfoo-shaped `tests` /
//!   `assert` / `defaultTest`) plus an optional JSONL dataset.
//! - [`Runner`]: runs every case against a [`Target`] (usually an
//!   [`EngineTarget`] around one compiled graph), concurrently, and scores it.
//! - [`assertion`]: deterministic output checks, latency and token budgets,
//!   agent trajectory checks and retrieval metrics, behind the [`Assertion`]
//!   trait so other crates can add types (e.g. an LLM judge).
//! - [`EvalReport`]: the summary, the gate verdict, and JSON / JUnit output.
//!
//! The harness is not part of the engine: it only consumes the run's event
//! stream, so runs evaluated here behave exactly like production runs.
//!
//! ```no_run
//! # async fn demo(engine: graphrt_core::Engine) -> Result<(), graphrt_eval::EvalError> {
//! use graphrt_eval::{EngineTarget, EvalSuite, Runner, RunnerOptions};
//!
//! let suite = EvalSuite::load(std::path::Path::new("evals/hello.eval.yaml"))?;
//! let report = Runner::new(RunnerOptions::default())
//!     .run(&suite, &EngineTarget::new(engine), |result| eprintln!("{}", graphrt_eval::report::format_result_line(result)))
//!     .await?;
//! assert!(report.gate_passed(), "{}", graphrt_eval::report::format_summary(&report));
//! # Ok(())
//! # }
//! ```

pub mod assertion;
pub mod capture;
pub mod config;
pub mod error;
mod glob;
pub mod path;
pub mod report;
pub mod runner;
pub mod schema;

pub use assertion::{Assertion, AssertionContext, AssertionRegistry, AssertionResult, Outcome};
pub use capture::{RetrievalHit, RunCapture, RunStatus, ToolCallRecord};
pub use config::{AssertionSpec, EvalSuite, Gate, TestCase};
pub use error::EvalError;
pub use report::{EvalReport, Summary, TestResult, TestStatus};
pub use runner::{EngineTarget, Runner, RunnerOptions, Target};
