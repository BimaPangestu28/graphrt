//! Command-line arguments.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Run agentic workflow graphs anywhere.
#[derive(Debug, Parser)]
#[command(name = "graphrt", version, about, propagate_version = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run a workflow and stream its output.
    Run(Box<RunArgs>),
    /// Parse and validate a workflow without running it.
    Validate(WorkflowArg),
    /// Show nodes, inputs, env variables and build compatibility of a workflow.
    Inspect(WorkflowArg),
    /// List the node kinds available in this build (the node catalog).
    Nodes(NodesArgs),
    /// Serve one workflow (file) or many (directory) over HTTP with SSE streaming.
    #[cfg(feature = "serve")]
    Serve(Box<ServeArgs>),
    /// Print version and enabled features.
    Version,
}

/// A workflow path.
#[derive(Debug, Args)]
pub struct WorkflowArg {
    /// Workflow document (.yaml, .yml or .json).
    pub workflow: PathBuf,
}

/// Options of `graphrt nodes`.
#[derive(Debug, Args)]
pub struct NodesArgs {
    /// Print the catalog (or one descriptor) as JSON, including config schemas.
    #[arg(long)]
    pub json: bool,
    /// Show only this node kind (full descriptor).
    #[arg(long, value_name = "KIND")]
    pub kind: Option<String>,
}

/// How `run` prints results.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum OutputMode {
    /// Live answer text, then the outputs.
    #[default]
    Text,
    /// Final result as one JSON object.
    Json,
    /// Every event as newline-delimited JSON.
    Events,
}

/// Options shared by `run` and `serve` for host services.
#[derive(Debug, Args, Clone, Default)]
pub struct ServiceArgs {
    /// Load environment variables from a dotenv file (model keys and workflow `env.*`).
    #[arg(long, value_name = "FILE")]
    pub env_file: Option<PathBuf>,
    /// MCP servers file (`{"mcpServers": {...}}`) exposing tools to tool/agent nodes.
    #[arg(long, value_name = "FILE")]
    pub mcp: Option<PathBuf>,
    /// Allow HTTP requests to private, loopback and link-local addresses.
    #[arg(long)]
    pub allow_private_network: bool,
    /// Model providers file (YAML or JSON) layered over the environment-based providers.
    /// Defaults to `$GRAPHRT_PROVIDERS` when set.
    #[arg(long, value_name = "FILE")]
    pub providers: Option<PathBuf>,
    /// Run `python3` code nodes in a local subprocess. NOT a security sandbox: only
    /// enable for trusted workflows.
    #[arg(long)]
    pub allow_python_subprocess: bool,
    /// Use this model (`provider/name`, e.g. `ollama/qwen2.5:14b`) for every model-backed
    /// node (llm, question-classifier, parameter-extractor, agent).
    #[arg(long, value_name = "PROVIDER/NAME")]
    pub model: Option<String>,
    /// Abort a run after this many milliseconds.
    #[arg(long, value_name = "MS")]
    pub timeout_ms: Option<u64>,
    /// Maximum node executions per run.
    #[arg(long, value_name = "N")]
    pub max_steps: Option<u64>,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Workflow document (.yaml, .yml or .json).
    pub workflow: PathBuf,
    /// Start input as `key=value` (string) or `key:=json`. Repeatable.
    #[arg(short, long = "input", value_name = "KEY=VALUE")]
    pub inputs: Vec<String>,
    /// JSON object of start inputs, from a file or `-` for stdin.
    #[arg(long, value_name = "FILE|-")]
    pub inputs_json: Option<String>,
    /// System variable (`sys.*`) as `key=value` or `key:=json`. Repeatable.
    #[arg(long = "sys", value_name = "KEY=VALUE")]
    pub sys: Vec<String>,
    /// Output format.
    #[arg(short, long, value_enum, default_value_t = OutputMode::Text)]
    pub output: OutputMode,
    /// Resume from a snapshot written by a paused run.
    #[arg(long, value_name = "SNAPSHOT")]
    pub resume: Option<PathBuf>,
    /// Input for a paused node as `node_id=json` (e.g. a submitted form). Repeatable.
    #[arg(long, value_name = "NODE=JSON")]
    pub resume_input: Vec<String>,
    /// Where to write the snapshot if the run pauses [default: <workflow>.snapshot.json].
    #[arg(long, value_name = "FILE")]
    pub snapshot_out: Option<PathBuf>,
    #[command(flatten)]
    pub services: ServiceArgs,
}

#[cfg(feature = "serve")]
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// A workflow file, or a directory of workflow files (each served under its file stem).
    pub path: PathBuf,
    /// Listen address.
    #[arg(long, default_value = "127.0.0.1:8080")]
    pub addr: String,
    /// Name of the environment variable holding the API key. When set, requests must send
    /// `Authorization: Bearer <key>`.
    #[arg(long, value_name = "VAR", default_value = "GRAPHRT_API_KEY")]
    pub api_key_env: String,
    /// Maximum request body size in bytes.
    #[arg(long, default_value_t = 2 * 1024 * 1024)]
    pub max_body_bytes: usize,
    #[command(flatten)]
    pub services: ServiceArgs,
}
