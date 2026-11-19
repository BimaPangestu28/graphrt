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
    /// Parse and validate a workflow without running it, reporting every problem.
    Validate(ValidateArgs),
    /// Print the JSON Schema of workflow documents for the node kinds in this
    /// build (including plugins). Point editors at it for completion and checks.
    Schema(SchemaArgs),
    /// Show nodes, inputs, env variables and build compatibility of a workflow.
    Inspect(WorkflowArg),
    /// List the node kinds available in this build (the node catalog).
    Nodes(NodesArgs),
    /// Serve one workflow (file) or many (directory) over HTTP with SSE streaming.
    #[cfg(feature = "serve")]
    Serve(Box<ServeArgs>),
    /// Ingest documents into, or query, a knowledge base (`--knowledge` config).
    #[cfg(feature = "rag")]
    Kb(Box<KbArgs>),
    /// Manage plugins: list, install, remove, inspect, scaffold.
    #[cfg(any(feature = "plugins", feature = "wasm"))]
    Plugin(PluginCmdArgs),
    /// Build, inspect, verify and sign portable bundles (`.grt`).
    #[cfg(feature = "bundle")]
    Bundle(BundleArgs),
    /// Opt in to or out of anonymous usage telemetry, or show exactly what it sends.
    #[cfg(feature = "telemetry")]
    Telemetry(TelemetryArgs),
    /// Print version and enabled features.
    Version,
}

/// Options of `graphrt telemetry`.
#[cfg(feature = "telemetry")]
#[derive(Debug, Args)]
pub struct TelemetryArgs {
    #[command(subcommand)]
    pub command: TelemetryCommand,
}

/// `graphrt telemetry` subcommands. Telemetry is off unless enabled; see docs/telemetry.md.
#[cfg(feature = "telemetry")]
#[derive(Debug, Subcommand)]
pub enum TelemetryCommand {
    /// Opt in: send one anonymous report a day (install id, version, OS/arch, features,
    /// daily run and request counts).
    Enable,
    /// Opt out and delete the install id and unsent counts.
    Disable,
    /// Show whether telemetry is on, why, and the exact next report.
    Status {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
}

/// Options of `graphrt bundle`.
#[cfg(feature = "bundle")]
#[derive(Debug, Args)]
pub struct BundleArgs {
    #[command(subcommand)]
    pub command: BundleCommand,
}

/// `graphrt bundle` subcommands.
#[cfg(feature = "bundle")]
#[derive(Debug, Subcommand)]
pub enum BundleCommand {
    /// Package a workflow with its WebAssembly plugins and configs into one `.grt` file.
    Build(Box<BundleBuildArgs>),
    /// Show a bundle's manifest, signature and compatibility with this build.
    Inspect {
        bundle: PathBuf,
        /// Print JSON.
        #[arg(long)]
        json: bool,
        /// Require a signature from this hex public key. Repeatable; also `$GRAPHRT_TRUSTED_KEYS`.
        #[arg(long = "trust-key", value_name = "HEX")]
        trust_key: Vec<String>,
    },
    /// Check integrity (and the signature, when trusted keys are given).
    Verify {
        bundle: PathBuf,
        /// Require a signature from this hex public key. Repeatable; also `$GRAPHRT_TRUSTED_KEYS`.
        #[arg(long = "trust-key", value_name = "HEX")]
        trust_key: Vec<String>,
    },
    /// Sign a bundle with an Ed25519 secret key file (from `graphrt bundle keygen`).
    Sign {
        bundle: PathBuf,
        /// Secret key file.
        #[arg(long, value_name = "FILE")]
        key: PathBuf,
        /// Output file [default: overwrite the bundle].
        #[arg(short, long, value_name = "FILE")]
        out: Option<PathBuf>,
    },
    /// Generate an Ed25519 signing key pair: writes the secret key file, prints the public key.
    Keygen {
        /// Secret key file to create (never overwritten).
        #[arg(long, value_name = "FILE")]
        out: PathBuf,
    },
}

/// Options of `graphrt bundle build`.
#[cfg(feature = "bundle")]
#[derive(Debug, Args)]
pub struct BundleBuildArgs {
    /// Workflow document to package (becomes the bundle entry).
    pub workflow: PathBuf,
    /// Output file [default: `<workflow>.grt` next to the workflow].
    #[arg(short, long, value_name = "FILE")]
    pub out: Option<PathBuf>,
    /// Bundle name [default: the workflow `name`, else its file stem].
    #[arg(long)]
    pub name: Option<String>,
    /// Bundle version.
    #[arg(long = "bundle-version", value_name = "VERSION", default_value = "0.1.0")]
    pub bundle_version: String,
    /// Include this plugin directory. WebAssembly plugins needed by the workflow are
    /// found automatically in the plugin search paths. Repeatable.
    #[arg(long = "plugin", value_name = "DIR")]
    pub plugin: Vec<PathBuf>,
    /// Knowledge base config to ship (memory store files it references are included).
    #[arg(long, value_name = "FILE")]
    pub knowledge: Option<PathBuf>,
    /// Model providers file to ship (must not contain inline keys).
    #[arg(long, value_name = "FILE")]
    pub providers: Option<PathBuf>,
    /// Directory copied under `assets/`. Repeatable.
    #[arg(long, value_name = "DIR")]
    pub include: Vec<PathBuf>,
    /// Sign the bundle with this secret key file.
    #[arg(long, value_name = "FILE")]
    pub sign_key: Option<PathBuf>,
    #[command(flatten)]
    pub plugins: PluginArgs,
}

/// Options of `graphrt kb`.
#[cfg(feature = "rag")]
#[derive(Debug, Args)]
pub struct KbArgs {
    /// Knowledge base config (YAML or JSON).
    #[arg(long, value_name = "FILE")]
    pub knowledge: PathBuf,
    /// Load environment variables (embedding API keys) from a dotenv file.
    #[arg(long, value_name = "FILE")]
    pub env_file: Option<PathBuf>,
    #[command(subcommand)]
    pub command: KbCommand,
}

/// `graphrt kb` subcommands.
#[cfg(feature = "rag")]
#[derive(Debug, clap::Subcommand)]
pub enum KbCommand {
    /// Extract, chunk, embed and store files (PDF, DOCX, XLSX, HTML, Markdown, ...).
    Ingest {
        /// Target dataset id.
        #[arg(long)]
        dataset: String,
        /// Replace chunks of documents that were ingested before.
        #[arg(long)]
        replace: bool,
        /// Files to ingest.
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// Retrieve the best-matching chunks for a query.
    Query {
        /// Dataset id(s). Repeatable.
        #[arg(long = "dataset", required = true)]
        dataset: Vec<String>,
        /// Number of chunks to return.
        #[arg(long, default_value_t = 4)]
        top_k: usize,
        /// The query text.
        query: String,
    },
}

/// A workflow path.
#[derive(Debug, Args)]
pub struct WorkflowArg {
    /// Workflow document (.yaml, .yml or .json).
    pub workflow: PathBuf,
    #[command(flatten)]
    pub plugins: PluginArgs,
}

/// Options of `graphrt validate`.
#[derive(Debug, Args)]
pub struct ValidateArgs {
    /// Workflow document (.yaml, .yml or .json).
    pub workflow: PathBuf,
    /// Output format.
    #[arg(long, value_enum, default_value_t = crate::diagnostics::DiagnosticFormat::Text)]
    pub format: crate::diagnostics::DiagnosticFormat,
    #[command(flatten)]
    pub plugins: PluginArgs,
}

/// Options of `graphrt schema`.
#[derive(Debug, Args)]
pub struct SchemaArgs {
    #[command(flatten)]
    pub plugins: PluginArgs,
}

/// Plugin search options shared by every command that builds graphs.
#[derive(Debug, Args, Clone, Default)]
pub struct PluginArgs {
    /// Plugin directory: one plugin, a directory of plugins, or a `.wasm` module.
    /// Searched before `$GRAPHRT_PLUGIN_PATH` and `~/.graphrt/plugins`. Repeatable.
    #[arg(long = "plugins", value_name = "DIR")]
    pub dirs: Vec<PathBuf>,
    /// Ignore `$GRAPHRT_PLUGIN_PATH` and `~/.graphrt/plugins`.
    #[arg(long)]
    pub no_default_plugins: bool,
    /// Trust a plugin by name: it may receive plaintext secrets and send host
    /// variables as HTTP bearer tokens when its manifest asks. Repeatable; also
    /// `$GRAPHRT_TRUSTED_PLUGINS` (comma-separated).
    #[arg(long = "trust-plugin", value_name = "NAME")]
    pub trusted: Vec<String>,
}

impl PluginArgs {
    /// Options for [`crate::extensions::Extensions::load`].
    pub fn options(&self) -> crate::extensions::PluginOptions {
        crate::extensions::PluginOptions {
            dirs: self.dirs.clone(),
            no_default: self.no_default_plugins,
            trusted: self.trusted.clone(),
        }
    }
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
    #[command(flatten)]
    pub plugins: PluginArgs,
}

/// Options of `graphrt plugin`.
#[derive(Debug, Args)]
pub struct PluginCmdArgs {
    #[command(subcommand)]
    pub command: PluginCommand,
}

/// Plugin languages supported by `graphrt plugin new`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum PluginLang {
    Python,
    Typescript,
    Rust,
}

/// `graphrt plugin` subcommands.
#[derive(Debug, Subcommand)]
pub enum PluginCommand {
    /// List plugins found in the plugin search paths.
    List {
        /// Print as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        plugins: PluginArgs,
    },
    /// Install a plugin from a local directory, a git URL or the plugin catalog.
    ///
    /// Examples:
    ///   graphrt plugin install open-meteo
    ///   graphrt plugin install open-meteo@0.1.0
    ///   graphrt plugin install ./acme
    ///   graphrt plugin install https://github.com/acme/graphrt-acme --rev v1.2.0
    ///   graphrt plugin install https://github.com/acme/plugins --rev v1.2.0 --subdir acme
    #[command(verbatim_doc_comment)]
    Install {
        /// Plugin directory, git URL (`https://...`, `git@...`, `*.git`), or a
        /// catalog plugin name (`name` or `name@version`).
        source: String,
        /// Branch or tag to check out (git sources).
        #[arg(long, value_name = "REV")]
        rev: Option<String>,
        /// Install the plugin in this subdirectory of the repository (git
        /// sources), for collections with one plugin per directory, e.g.
        /// `--subdir whatsapp-api`. Must be relative, without `..` or symlinks.
        #[arg(long, value_name = "PATH")]
        subdir: Option<String>,
        /// Install into this directory instead of `~/.graphrt/plugins`.
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
        /// Catalog index for plugin names: an https:// URL, a file:// URL or a
        /// path [default: $GRAPHRT_PLUGIN_INDEX, else the graphrt-plugins catalog release].
        #[arg(long, value_name = "URL|PATH")]
        index: Option<String>,
        /// Install a catalog plugin that is not labelled stable/beta with a
        /// none/low/medium ToS risk: experimental, high-risk, unrecognised or
        /// unlabelled (graphrt has no interactive prompt).
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Search the plugin catalog: name, version, status and description.
    ///
    /// Examples:
    ///   graphrt plugin search
    ///   graphrt plugin search weather
    #[command(verbatim_doc_comment)]
    Search {
        /// Case-insensitive text to find in plugin names and descriptions.
        query: Option<String>,
        /// Catalog index: an https:// URL, a file:// URL or a path
        /// [default: $GRAPHRT_PLUGIN_INDEX, else the graphrt-plugins catalog release].
        #[arg(long, value_name = "URL|PATH")]
        index: Option<String>,
    },
    /// Remove an installed plugin.
    Remove {
        /// Plugin name.
        name: String,
        /// Plugin install directory [default: ~/.graphrt/plugins].
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
    },
    /// Show a plugin's manifest details and node descriptors.
    Info {
        /// Plugin name.
        name: String,
        /// Print as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        plugins: PluginArgs,
    },
    /// Scaffold a new plugin in `<dir>/<name>`.
    New {
        /// Plugin name (lowercase letters, digits, `-`, `_`).
        name: String,
        /// Implementation language.
        #[arg(long, value_enum, default_value_t = PluginLang::Python)]
        lang: PluginLang,
        /// Parent directory [default: current directory].
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
    },
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
    /// Knowledge base config (YAML or JSON) for knowledge-retrieval nodes.
    #[arg(long, value_name = "FILE")]
    pub knowledge: Option<PathBuf>,
    /// Directory serving `local_file` references (e.g. uploaded documents).
    #[arg(long, value_name = "DIR")]
    pub files_dir: Option<PathBuf>,
    /// Abort a run after this many milliseconds.
    #[arg(long, value_name = "MS")]
    pub timeout_ms: Option<u64>,
    /// Maximum node executions per run.
    #[arg(long, value_name = "N")]
    pub max_steps: Option<u64>,
    /// Export OpenTelemetry traces and metrics over OTLP/HTTP (`$OTEL_EXPORTER_OTLP_ENDPOINT`,
    /// default http://localhost:4318). Enabled automatically when that variable is set.
    #[arg(long)]
    pub otel: bool,
    /// Print OpenTelemetry spans as JSON lines on stderr (debugging).
    #[arg(long)]
    pub otel_stdout: bool,
    /// Require bundles (`.grt`) to be signed by this hex public key. Repeatable; also
    /// `$GRAPHRT_TRUSTED_KEYS` (comma-separated).
    #[arg(long = "trust-key", value_name = "HEX")]
    pub trust_key: Vec<String>,
    #[command(flatten)]
    pub plugins: PluginArgs,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Workflow document (.yaml, .yml or .json) or bundle (.grt).
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
    /// Maximum size in bytes of a knowledge base file upload
    /// (`PUT /v1/knowledge/datasets/{dataset}/documents/{document}/file`).
    #[arg(long, default_value_t = 16 * 1024 * 1024)]
    pub max_upload_bytes: usize,
    /// Directory for durable run records, events and snapshots (paused runs survive
    /// restarts and can be resumed by id). Defaults to `$GRAPHRT_STORE`, else in memory.
    #[arg(long, value_name = "DIR")]
    pub store: Option<PathBuf>,
    /// Delete stored runs (except running ones) not updated for this many days.
    #[arg(long, value_name = "DAYS")]
    pub store_ttl_days: Option<u64>,
    /// In-memory store only: keep at most this many runs; the oldest finished runs are
    /// dropped first, running and paused runs are always kept (0 = unlimited). Lower it
    /// on small devices: a finished run takes roughly 10-20 KiB.
    #[arg(long, value_name = "N", default_value_t = crate::store::DEFAULT_MAX_RUNS)]
    pub max_runs: usize,
    /// Webhook triggers file (YAML or JSON): `POST /v1/hooks/{hook}` starts a run from a
    /// JSON delivery, with optional signature checks, idempotency keys and completion
    /// callbacks. See docs/serving.md.
    #[arg(long, value_name = "FILE")]
    pub webhooks: Option<PathBuf>,
    /// Maximum number of runs executing at the same time (started or resumed); further
    /// requests get `429 Too Many Requests` (0 = unlimited). Paused runs do not count.
    #[arg(long, value_name = "N", default_value_t = crate::serve::DEFAULT_MAX_ACTIVE_RUNS)]
    pub max_active_runs: usize,
    #[command(flatten)]
    pub services: ServiceArgs,
}
