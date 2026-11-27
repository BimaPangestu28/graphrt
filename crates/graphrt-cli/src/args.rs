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
    /// Evaluate a workflow against the test cases of an eval file and gate CI on the
    /// pass rate (exit code 5 when the gate fails).
    #[cfg(feature = "eval")]
    Eval(Box<EvalArgs>),
    /// Serve one workflow (file) or many (directory) over HTTP with SSE streaming.
    #[cfg(feature = "serve")]
    Serve(Box<ServeArgs>),
    /// Ingest documents into, or query, a knowledge base (`--knowledge` config).
    #[cfg(feature = "rag")]
    Kb(Box<KbArgs>),
    /// Manage plugins: list, install, remove, inspect, scaffold.
    #[cfg(any(feature = "plugins", feature = "wasm"))]
    Plugin(PluginCmdArgs),
    /// Manage agent skills: list, inspect, install, grant tools, remove.
    #[cfg(feature = "skills")]
    Skill(SkillCmdArgs),
    /// Review what agents learned: list, approve, edit, forget.
    #[cfg(feature = "learning")]
    Learn(LearnCmdArgs),
    /// What agents remember about users: list, approve, export, erase a user, audit.
    #[cfg(feature = "memory")]
    Memory(MemoryCmdArgs),
    /// Build, inspect, verify and sign portable bundles (`.grt`).
    #[cfg(feature = "bundle")]
    Bundle(BundleArgs),
    /// Opt in to or out of anonymous usage telemetry, or show exactly what it sends.
    #[cfg(feature = "telemetry")]
    Telemetry(TelemetryArgs),
    /// Run the gateway: the always-on front of scale-to-zero agents (store,
    /// auth, limits, webhooks, cron, wake/scale). `--dev <workflow>` for a
    /// zero-config local setup.
    #[cfg(feature = "gateway")]
    Gateway(Box<crate::gateway_cmd::GatewayArgs>),
    /// Deploy a new immutable version of a deployment, through a gateway or
    /// with `--direct` straight to Fly.io; manage direct deployments
    /// (status, list-versions, rollback, pause, resume, remove).
    // `--version` names the deployment version here, not the binary's.
    #[cfg(any(feature = "gateway", feature = "deploy-fly", feature = "deploy-cloud-run"))]
    #[command(disable_version_flag = true)]
    Deploy(Box<crate::deploy_cmd::DeployArgs>),
    /// Run the collaboration server: team workspaces where people edit agent
    /// projects together in real time (hosted or self hosted). See docs/collab.md.
    #[cfg(feature = "collab")]
    Collab(crate::collab_cmd::CollabArgs),
    /// Print version and enabled features.
    Version,
    /// Internal: an agent a gateway starts ahead of demand; it waits for one
    /// assignment (`serve` arguments and gateway keys) on stdin.
    #[cfg(feature = "gateway")]
    #[command(name = "agent-standby", hide = true)]
    AgentStandby,
    /// Internal: tooling run inside a deployed Machine (e.g. writing an agent onto its volume).
    #[cfg(feature = "bundle")]
    #[command(name = "internal", hide = true, subcommand)]
    Internal(crate::store_file::InternalCommand),
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
    /// Sync the `sources:` of the knowledge config now (web sites, APIs, Google
    /// Drive, Notion, SharePoint): only what changed is indexed again, and what
    /// was deleted at the source is removed.
    #[cfg(feature = "connectors")]
    Sync {
        /// Only this source (repeatable). Every source when left out.
        #[arg(long = "source")]
        source: Vec<String>,
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
            pool: crate::extensions::PluginPool::default(),
            allow_private_network: false,
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

/// Options of `graphrt skill`.
#[cfg(feature = "skills")]
#[derive(Debug, Args)]
pub struct SkillCmdArgs {
    #[command(subcommand)]
    pub command: SkillCommand,
    /// Skill store directory [default: $GRAPHRT_SKILLS_DIR, else ~/.graphrt/skills].
    #[arg(long, global = true, value_name = "DIR")]
    pub dir: Option<PathBuf>,
    /// Print as JSON.
    #[arg(long, global = true)]
    pub json: bool,
}

/// Tools to grant to a skill.
#[cfg(feature = "skills")]
#[derive(Debug, Args)]
pub struct SkillGrantArgs {
    /// Grant a tool the skill asks for (`provider/tool`). Every call waits for approval.
    #[arg(long = "grant", value_name = "TOOL")]
    pub ask: Vec<String>,
    /// Grant a tool the skill asks for and let its calls run without asking.
    #[arg(long = "allow", value_name = "TOOL")]
    pub allow: Vec<String>,
}

/// `graphrt skill` subcommands.
#[cfg(feature = "skills")]
#[derive(Debug, Subcommand)]
pub enum SkillCommand {
    /// List installed skills.
    List,
    /// Show a skill before or after installing: what it does, the tools it
    /// asks for, its files and what the scan found.
    Inspect {
        /// An installed skill's name, or a directory with a `SKILL.md` (or an
        /// Agent Plugins package with `plugin.json` and `skills/`).
        skill: String,
    },
    /// Install a skill from a directory.
    ///
    /// A skill gets no tool unless you grant it here or later with `graphrt skill grant`.
    ///
    /// Examples:
    ///   graphrt skill inspect ./refund-policy
    ///   graphrt skill install ./refund-policy
    ///   graphrt skill install ./weekly-report --grant mail/send
    ///   graphrt skill install ./weekly-report --replace
    ///   graphrt skill install kebijakan-refund          (from the signed catalog)
    ///   graphrt skill install kebijakan-refund@1.0.0
    #[command(verbatim_doc_comment)]
    Install {
        /// Directory with a `SKILL.md`, an Agent Plugins package, or a catalog
        /// skill name (`name` or `name@version`). A catalog skill is installed
        /// only from an index whose signature verifies.
        source: String,
        /// Catalog index (https:// URL, file:// URL or path) [default: the graphrt-plugins catalog].
        #[arg(long, value_name = "URL|PATH")]
        index: Option<String>,
        /// Public key the catalog index must be signed with, instead of the
        /// graphrt-plugins key: a base64 minisign key or a `.pub` file.
        #[arg(long, value_name = "KEY|FILE")]
        index_key: Option<String>,
        #[command(flatten)]
        grants: SkillGrantArgs,
        /// Install although the scan found something that needs a decision.
        #[arg(long)]
        accept_findings: bool,
        /// Replace an installed skill of the same name. Its earlier grants are not kept.
        #[arg(long)]
        replace: bool,
    },
    /// List the skills of the signed catalog whose name or description contains QUERY.
    Search {
        query: Option<String>,
        /// Catalog index (https:// URL, file:// URL or path) [default: the graphrt-plugins catalog].
        #[arg(long, value_name = "URL|PATH")]
        index: Option<String>,
        /// Public key the catalog index must be signed with, instead of the graphrt-plugins key.
        #[arg(long, value_name = "KEY|FILE")]
        index_key: Option<String>,
    },
    /// Write a new skill from a description in plain language or from an
    /// example conversation, with a model. The result is a folder to read,
    /// edit and then install with `graphrt skill install`.
    ///
    /// Examples:
    ///   graphrt skill draft --model openai/gpt-4.1 --describe "When a customer asks for a refund, check the order date first"
    ///   graphrt skill draft --model anthropic/claude-sonnet-4-5 --conversation chat.json --tool orders/lookup
    #[command(verbatim_doc_comment)]
    Draft {
        /// The model that writes the draft (`provider/name`).
        #[arg(long, value_name = "PROVIDER/NAME")]
        model: String,
        /// What the skill should do, in plain language.
        #[arg(long, value_name = "TEXT", conflicts_with = "conversation", required_unless_present = "conversation")]
        describe: Option<String>,
        /// A JSON file with an example conversation: `[{"role": "user", "content": "..."}, ...]`.
        #[arg(long, value_name = "FILE")]
        conversation: Option<PathBuf>,
        /// A tool the skill may ask for (`provider/tool`). Asking grants nothing.
        #[arg(long = "tool", value_name = "TOOL")]
        tools: Vec<String>,
        /// Directory to write `<name>/SKILL.md` into.
        #[arg(long, value_name = "DIR", default_value = ".")]
        out: PathBuf,
        /// Model providers file (YAML or JSON) layered over the environment-based providers.
        #[arg(long, value_name = "FILE")]
        providers: Option<PathBuf>,
    },
    /// Replace the tools granted to an installed skill (no flags: grant nothing).
    Grant {
        /// Skill name.
        name: String,
        #[command(flatten)]
        grants: SkillGrantArgs,
    },
    /// Let agents use an installed skill again.
    Enable {
        /// Skill name.
        name: String,
    },
    /// Stop agents from using an installed skill, without removing it.
    Disable {
        /// Skill name.
        name: String,
    },
    /// Remove an installed skill.
    Remove {
        /// Skill name.
        name: String,
    },
}

/// Options of `graphrt memory`.
#[cfg(feature = "memory")]
#[derive(Debug, Args)]
pub struct MemoryCmdArgs {
    #[command(subcommand)]
    pub command: MemoryCommand,
    /// Memory store directory [default: $GRAPHRT_MEMORY_DIR, else ~/.graphrt/memory].
    #[arg(long, global = true, value_name = "DIR")]
    pub dir: Option<PathBuf>,
    /// The tenant, for a server started with `--memory-tenant`.
    #[arg(long, global = true, value_name = "NAME")]
    pub tenant: Option<String>,
    /// Lesson store whose lessons `forget-user` erases too [default:
    /// $GRAPHRT_LEARNING_DIR, else ~/.graphrt/learning].
    #[arg(long, global = true, value_name = "DIR")]
    pub learning_dir: Option<PathBuf>,
    /// Print as JSON.
    #[arg(long, global = true)]
    pub json: bool,
    /// Who is making the change, for the audit log.
    #[arg(long, global = true, value_name = "NAME", default_value = "owner")]
    pub actor: String,
}

/// `graphrt memory` subcommands. `AGENT` is `long_term_memory.agent`, by default the workflow name.
#[cfg(feature = "memory")]
#[derive(Debug, Subcommand)]
pub enum MemoryCommand {
    /// List the notes of a user, or of the agent itself without `--user`.
    List {
        agent: String,
        #[arg(long, value_name = "USER")]
        user: Option<String>,
        /// Only notes waiting for review.
        #[arg(long)]
        pending: bool,
        /// Only notes that match these words.
        #[arg(long, value_name = "WORDS")]
        query: Option<String>,
    },
    /// Add a note yourself: it is used from the next run on.
    Add {
        agent: String,
        text: String,
        #[arg(long, value_name = "USER")]
        user: Option<String>,
    },
    /// Approve a note waiting for review.
    Approve {
        agent: String,
        id: String,
        #[arg(long, value_name = "USER")]
        user: Option<String>,
    },
    /// Reject a note waiting for review: it is removed.
    Reject {
        agent: String,
        id: String,
        #[arg(long, value_name = "USER")]
        user: Option<String>,
    },
    /// Remove one note.
    Forget {
        agent: String,
        id: String,
        #[arg(long, value_name = "USER")]
        user: Option<String>,
    },
    /// Print everything kept about a user, as JSON.
    Export { agent: String, user: String },
    /// Erase everything kept about a user (notes, and lessons in the lesson store).
    ForgetUser { agent: String, user: String },
    /// Print the audit log of an agent (no note text is in it).
    Audit {
        agent: String,
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    /// Read the notes of a user into the memory graph again, with a model,
    /// or print the graph.
    #[cfg(feature = "memory-graph")]
    Graph {
        #[command(subcommand)]
        command: MemoryGraphCommand,
    },
}

/// `graphrt memory graph` subcommands.
#[cfg(feature = "memory-graph")]
#[derive(Debug, Subcommand)]
pub enum MemoryGraphCommand {
    /// Read every note of a user (in use and waiting) into the graph with MODEL (`provider/name`).
    Build {
        agent: String,
        #[arg(long, value_name = "USER")]
        user: Option<String>,
        #[arg(long, value_name = "PROVIDER/NAME")]
        model: String,
        /// Model providers file (default: `$GRAPHRT_PROVIDERS` and the environment).
        #[arg(long, value_name = "FILE")]
        providers: Option<PathBuf>,
    },
    /// Print the entities and relations of a user, or of the agent without `--user`.
    Show {
        agent: String,
        #[arg(long, value_name = "USER")]
        user: Option<String>,
    },
}

/// Options of `graphrt learn`.
#[cfg(feature = "learning")]
#[derive(Debug, Args)]
pub struct LearnCmdArgs {
    #[command(subcommand)]
    pub command: LearnCommand,
    /// Lesson store directory [default: $GRAPHRT_LEARNING_DIR, else ~/.graphrt/learning].
    #[arg(long, global = true, value_name = "DIR")]
    pub dir: Option<PathBuf>,
    /// Print as JSON.
    #[arg(long, global = true)]
    pub json: bool,
    /// Who is making the change, for the audit log.
    #[arg(long, global = true, value_name = "NAME", default_value = "owner")]
    pub actor: String,
}

/// Lesson states accepted by `graphrt learn list --status`.
#[cfg(feature = "learning")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum LessonStatusArg {
    Proposed,
    Approved,
    Rejected,
    Retired,
}

/// Lesson kinds accepted by `graphrt learn add --kind`.
#[cfg(feature = "learning")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum LessonKindArg {
    Preference,
    Fact,
    Procedure,
    Correction,
}

/// `graphrt learn` subcommands. `AGENT` is the name an `agent` node gives in `learning.agent`.
#[cfg(feature = "learning")]
#[derive(Debug, Subcommand)]
pub enum LearnCommand {
    /// List what an agent learned, with where each lesson came from.
    List {
        agent: String,
        /// Only the lessons of this user.
        #[arg(long, value_name = "USER", conflicts_with = "shared")]
        user: Option<String>,
        /// Only the lessons that apply to every run of the agent.
        #[arg(long)]
        shared: bool,
        /// Only lessons in this state.
        #[arg(long, value_enum)]
        status: Option<LessonStatusArg>,
    },
    /// Add a lesson yourself. It is used from the next run on.
    Add {
        agent: String,
        /// The lesson, as one short statement.
        text: String,
        /// Apply it to this user only.
        #[arg(long, value_name = "USER")]
        user: Option<String>,
        #[arg(long, value_enum, default_value = "preference")]
        kind: LessonKindArg,
    },
    /// Approve a proposed lesson: it is used from the next run on.
    Approve { agent: String, id: String },
    /// Reject a proposed lesson. The same text is not proposed again.
    Reject { agent: String, id: String },
    /// Stop using an approved lesson, keeping it on record.
    Retire { agent: String, id: String },
    /// Change the text of a lesson.
    Edit { agent: String, id: String, text: String },
    /// Make a lesson of one user apply to every run of the agent.
    Promote { agent: String, id: String },
    /// Delete a lesson for good.
    Delete { agent: String, id: String },
    /// Delete every lesson of a user, or every lesson that came from a run.
    Forget {
        agent: String,
        #[arg(long, value_name = "USER", required_unless_present = "run", conflicts_with = "run")]
        user: Option<String>,
        #[arg(long, value_name = "RUN_ID")]
        run: Option<String>,
    },
    /// Record feedback on a run. Nothing is learned until `graphrt learn distill`.
    Feedback {
        agent: String,
        /// The run the feedback is about.
        #[arg(long, value_name = "RUN_ID")]
        run: String,
        /// What the person said should be different.
        #[arg(long)]
        comment: String,
        /// The user the run was for, and the author of the feedback.
        #[arg(long, value_name = "USER")]
        user: Option<String>,
        /// The user's identity was verified (a signed in user). Without it the
        /// feedback is treated like any other unverified content.
        #[arg(long, requires = "user")]
        authenticated: bool,
    },
    /// Turn recorded feedback into lesson proposals. With `--model`, a model
    /// also suggests lessons from the recorded runs. Those always wait for
    /// approval.
    Distill {
        agent: String,
        /// A model that suggests lessons (`provider/name`).
        #[arg(long, value_name = "PROVIDER/NAME")]
        model: Option<String>,
        /// Model providers file (YAML or JSON) layered over the environment-based providers.
        #[arg(long, value_name = "FILE", requires = "model")]
        providers: Option<PathBuf>,
    },
    /// Write a skill draft from the procedures the agent learned (approved
    /// lessons that apply to every run). The folder is then reviewed and
    /// installed with `graphrt skill install`.
    Skill {
        agent: String,
        /// The model that writes the draft (`provider/name`).
        #[arg(long, value_name = "PROVIDER/NAME")]
        model: String,
        /// A tool the skill may ask for (`provider/tool`). Asking grants nothing.
        #[arg(long = "tool", value_name = "TOOL")]
        tools: Vec<String>,
        /// Directory to write `<name>/SKILL.md` into.
        #[arg(long, value_name = "DIR", default_value = ".")]
        out: PathBuf,
        /// Model providers file (YAML or JSON) layered over the environment-based providers.
        #[arg(long, value_name = "FILE")]
        providers: Option<PathBuf>,
    },
    /// Show the agent's learning policy, or change automatic approval.
    Policy {
        agent: String,
        /// Approve low risk feedback of an authenticated user for that user's
        /// own runs without waiting for the owner.
        #[arg(long, value_name = "BOOL")]
        auto_approve: Option<bool>,
    },
    /// Show the history of changes to an agent's lessons.
    Audit { agent: String },
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
        /// Minisign public key the catalog index must be signed with, instead
        /// of the built-in graphrt-plugins catalog key: the base64 key or the
        /// path of a .pub file [default: $GRAPHRT_PLUGIN_INDEX_KEY].
        #[arg(long, value_name = "KEY|PATH")]
        index_key: Option<String>,
        /// Use a local index file that has no signature (catalog development).
        /// Refused for https:// indexes.
        #[arg(long, conflicts_with = "index_key")]
        allow_unsigned_index: bool,
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
        /// Minisign public key the catalog index must be signed with, instead
        /// of the built-in graphrt-plugins catalog key: the base64 key or the
        /// path of a .pub file [default: $GRAPHRT_PLUGIN_INDEX_KEY].
        #[arg(long, value_name = "KEY|PATH")]
        index_key: Option<String>,
        /// Use a local index file that has no signature (catalog development).
        /// Refused for https:// indexes.
        #[arg(long, conflicts_with = "index_key")]
        allow_unsigned_index: bool,
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
    /// Give agents a file workspace in this directory (created when missing): the
    /// `files` tools (`files/list`, `read`, `search`, `write`, `info`, `delete`) work
    /// inside it and nowhere else. See docs/files-and-images.md.
    #[cfg(feature = "files")]
    #[arg(long, value_name = "DIR")]
    pub workspace: Option<PathBuf>,
    /// Directory of the long term memory of `agent` nodes (`long_term_memory`).
    /// Default `$GRAPHRT_MEMORY_DIR`, else `~/.graphrt/memory`.
    #[cfg(feature = "memory")]
    #[arg(long, value_name = "DIR")]
    pub memory_dir: Option<PathBuf>,
    /// Keep every memory scope of this process under one tenant: its own
    /// directory, erased by removing it. Letters, digits, `.`, `_` and `-`.
    #[cfg(feature = "memory")]
    #[arg(long, value_name = "NAME")]
    pub memory_tenant: Option<String>,
    /// Also recall notes by meaning: `openai/MODEL` (an embeddings model of
    /// the `openai` provider, `OPENAI_API_KEY` and `OPENAI_BASE_URL`) or `local`
    /// (the built in local model, downloaded on first use). Without it recall
    /// is by keyword only.
    #[cfg(feature = "memory")]
    #[arg(long, value_name = "SPEC")]
    pub memory_embedder: Option<String>,
    /// The workspace cannot be changed: no `files/write` and no `files/delete`.
    #[cfg(feature = "files")]
    #[arg(long, requires = "workspace")]
    pub workspace_read_only: bool,
    /// One private directory per user: agent nodes must send a scope with
    /// `tool_context: { files: { scope: ... } }`, and calls without one are refused.
    #[cfg(feature = "files")]
    #[arg(long, requires = "workspace")]
    pub workspace_per_user: bool,
    /// Add the `image` tools to the workspace: `image/info`, `view` (the model
    /// sees the image), `resize`, `convert` and `thumbnail`.
    #[cfg(feature = "image")]
    #[arg(long, requires = "workspace")]
    pub image_tools: bool,
    /// Add `image/generate`, backed by this image model. `NAME` or `openai/NAME`
    /// (for example `gpt-image-2`) uses the OpenAI images API: key from
    /// `$OPENAI_API_KEY`, endpoint from `$OPENAI_BASE_URL`. `gemini/NAME` (for
    /// example `gemini/gemini-3.1-flash-image`) uses the Gemini API: key from
    /// `$GEMINI_API_KEY` or `$GOOGLE_API_KEY`, endpoint from `$GEMINI_BASE_URL`.
    #[cfg(feature = "image")]
    #[arg(long, value_name = "[PROVIDER/]NAME", requires = "image_tools")]
    pub image_model: Option<String>,
    /// Add the `documents` tools to the workspace: `documents/create_pdf`,
    /// `create_docx` and `create_xlsx` write PDF, Word and Excel files.
    #[cfg(feature = "documents")]
    #[arg(long, requires = "workspace")]
    pub document_tools: bool,
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
    /// Export to a vendor over OTLP with its endpoint, headers and resource attributes
    /// filled from its own environment variables: langfuse, langsmith, braintrust, opik,
    /// phoenix, mlflow (see docs/observability.md). Also `$GRAPHRT_OTEL_PRESET`.
    /// Implies `--otel`.
    #[cfg(feature = "otel")]
    #[arg(long, value_name = "NAME")]
    pub otel_preset: Option<String>,
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
    /// Session / conversation id stamped on the run's OpenTelemetry spans.
    #[arg(long, value_name = "ID")]
    pub session_id: Option<String>,
    /// End-user id stamped on the run's OpenTelemetry spans.
    #[arg(long, value_name = "ID")]
    pub user_id: Option<String>,
    /// Tag for the run's OpenTelemetry spans. Repeatable.
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,
    /// Metadata `key=value` for the run's OpenTelemetry spans. Repeatable.
    #[arg(long = "metadata", value_name = "KEY=VALUE")]
    pub metadata: Vec<String>,
    #[command(flatten)]
    pub services: ServiceArgs,
}

/// Options of `graphrt eval`.
#[cfg(feature = "eval")]
#[derive(Debug, Args)]
pub struct EvalArgs {
    /// Eval file (`*.eval.yaml`): workflow, test cases, assertions and gate.
    pub file: PathBuf,
    /// Tests running at the same time.
    #[arg(short = 'j', long = "concurrency", value_name = "N", default_value_t = 4)]
    pub concurrency: usize,
    /// Only run tests whose id or a tag matches (`*` wildcards). Repeatable.
    #[arg(long, value_name = "PATTERN")]
    pub filter: Vec<String>,
    /// Run every test N times (each trial is reported) and report pass@k / pass^k per test.
    #[arg(long, visible_alias = "repeat", value_name = "N", default_value_t = 1)]
    pub trials: u32,
    /// Write the full results as JSON (schema in docs/evals.md); the file serves as a later `--baseline`.
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,
    /// Compare with earlier `--output` results; a regression beyond `gate.baseline` tolerances fails the gate.
    #[arg(long, value_name = "FILE")]
    pub baseline: Option<PathBuf>,
    /// Point every model node of the judge workflows at PROVIDER/NAME (`--model` never touches judges).
    #[arg(long, value_name = "PROVIDER/NAME")]
    pub judge_model: Option<String>,
    /// Write the results as JUnit XML (CI test reports).
    #[arg(long, value_name = "FILE")]
    pub junit: Option<PathBuf>,
    /// Required pass rate (0..1), overriding the eval file's `gate.pass_rate`.
    #[arg(long, value_name = "RATE")]
    pub pass_rate: Option<f64>,
    /// Exit code when the gate fails.
    #[arg(long, value_name = "CODE", default_value_t = crate::error::EXIT_GATE_FAILED)]
    pub fail_exit_code: u8,
    /// Include every run event (except token chunks) in the JSON results.
    #[arg(long)]
    pub include_events: bool,
    /// `--timeout-ms` applies to each test run. `--providers`, `--knowledge`, `--mcp`
    /// and `--plugins` add to (or override) the eval file's settings.
    #[command(flatten)]
    pub services: ServiceArgs,
}

/// Who may use the workspace routes of `graphrt serve` (`--workspace-api`).
#[cfg(all(feature = "serve", feature = "files"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum WorkspaceApi {
    /// Holders of the API key.
    Key,
    /// Holders of the API key, and verified end users inside their own directory.
    Users,
}

#[cfg(feature = "serve")]
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// A workflow file, or a directory of workflow files (each served under its file stem).
    pub path: PathBuf,
    /// Listen address: `host:port`, `unix:/path/to.sock` for a Unix socket
    /// (created mode 0600, e.g. for a gateway on the same host), or on Windows
    /// `pipe:\\.\pipe\name` for a named pipe (local clients only).
    #[arg(long, visible_alias = "listen", value_name = "ADDR", default_value = "127.0.0.1:8080")]
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
    /// Serve the workspace over HTTP under `/v1/files` (list, upload, download,
    /// delete). `key`: for the API key. `users`: also for verified end users
    /// (`--verify-jwt`), each inside their own directory (`--workspace-per-user`).
    /// Off unless given. Uploads are limited by `--max-upload-bytes`.
    #[cfg(feature = "files")]
    #[arg(long, value_enum, value_name = "WHO", requires = "workspace")]
    pub workspace_api: Option<WorkspaceApi>,
    /// Directory for durable run records, events and snapshots (paused runs survive
    /// restarts and can be resumed by id). Defaults to `$GRAPHRT_STORE`, else in memory.
    #[arg(long, value_name = "DIR")]
    pub store: Option<PathBuf>,
    /// Delete stored runs (except running ones) not updated for this many days.
    #[arg(long, value_name = "DAYS")]
    pub store_ttl_days: Option<u64>,
    /// Keep each run's inputs (as the start node accepted them, secrets masked) in its
    /// run record, e.g. to turn production runs into evaluation cases. Off by default.
    #[arg(long)]
    pub store_inputs: bool,
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
    /// Keep N processes of each process plugin started from startup (up to 32), so
    /// no run waits for a plugin to start (a Node.js host takes ~50 ms); a process
    /// that exits is replaced in the background, and requests spread over the N
    /// processes. Each costs the plugin's memory (a Node.js host ~40 MiB). 0 (the
    /// default): one process, started at the first plugin node (behind a gateway:
    /// at startup). See docs/plugins.md.
    #[cfg(feature = "plugins")]
    #[arg(long, value_name = "N", default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=32))]
    pub plugin_pool: u8,
    /// With `--plugin-pool`: stop a plugin's processes after this long without a
    /// request, freeing their memory; the next request, or a gateway warm hint,
    /// starts them again. Accepts `500ms`, `30s`, `5m` or plain seconds. Default: never.
    #[cfg(feature = "plugins")]
    #[arg(long, value_name = "DURATION", value_parser = crate::drain::parse_duration)]
    pub plugin_idle: Option<std::time::Duration>,
    /// On SIGTERM / SIGINT, finish within this long: new runs get `503`, active runs
    /// pause (in-flight nodes are cancelled once the deadline nears and run again on
    /// resume), their state is stored, then the process exits. Set it a few seconds
    /// below the platform's grace period (Kubernetes `terminationGracePeriodSeconds`,
    /// Fly `kill_timeout`, ...). Accepts `500ms`, `4s`, `2m` or plain seconds.
    #[arg(long, value_name = "DURATION", default_value = "4s", value_parser = crate::drain::parse_duration)]
    pub drain_timeout: std::time::Duration,
    /// Do not resume, at startup, the async and webhook runs a previous shutdown
    /// drained (they stay paused until resumed over the API).
    #[arg(long)]
    pub no_resume_drained: bool,
    /// Map a request header to a start input as `INPUT=HEADER` (e.g.
    /// `user_jwt=X-User-Authorization`), for per-user credentials such as a JWT. Applies
    /// to workflows whose start node declares the input, which must be `type: secret`; a
    /// leading `Bearer ` is stripped. Resume requests must send the header again.
    /// Repeatable. See docs/serving.md.
    #[arg(long, value_name = "INPUT=HEADER")]
    pub input_header: Vec<crate::header_inputs::HeaderInputSpec>,
    /// Let pages of this origin call the API from a browser (CORS), e.g.
    /// `https://app.example.com`. Repeatable [env: GRAPHRT_CORS_ORIGINS, commas].
    /// Off unless given. Origins are matched exactly (scheme, host, port); `*`
    /// allows any origin and needs an API key. Cookies are never accepted from
    /// another origin: pages send their token in a header. See docs/web.md.
    #[cfg(feature = "cors")]
    #[arg(long, value_name = "ORIGIN")]
    pub cors_origin: Vec<String>,
    /// Let a verified end user (or a chat guest) answer the questions of their own
    /// paused runs (`POST /v1/runs/{id}/resume`). Off by default: a paused run can
    /// be an approval that someone else has to give, such as a tool behind
    /// `require_approval`. Turn it on only when every pause of the workflows that
    /// users start is a question to that user.
    #[cfg(feature = "jwt")]
    #[arg(long)]
    pub user_resume: bool,
    #[cfg(feature = "conversations")]
    #[command(flatten)]
    pub conversations: ConversationArgs,
    #[cfg(feature = "web-chat")]
    #[command(flatten)]
    pub chat: ChatArgs,
    #[cfg(feature = "devices")]
    #[command(flatten)]
    pub monitor: MonitorArgs,
    #[cfg(feature = "channels")]
    #[command(flatten)]
    pub channels: ChannelArgs,
    /// Serve the admin console at `/console/` with this config (YAML or JSON)
    /// [env: GRAPHRT_CONSOLE]: people sign in with your OpenID Connect provider,
    /// get a role (owner, operator, viewer) checked on every route, and manage
    /// secrets and workflow versions per environment; what they do goes to an
    /// audit log. Needs the API key, which keeps working for automation. Off
    /// unless given. See docs/console.md.
    #[cfg(feature = "console")]
    #[arg(long, value_name = "FILE")]
    pub console: Option<PathBuf>,
    /// Online evaluation: an evaluator config (YAML or JSON) that scores a deterministic
    /// sample of finished runs with judge workflows, regexes, JSON schemas, budgets or
    /// trajectory checks, in a bounded background pool. Scores are stored with each run
    /// (`GET /v1/runs/{id}/scores`). See docs/evals.md.
    #[cfg(feature = "online-eval")]
    #[arg(long, value_name = "FILE")]
    pub evaluators: Option<PathBuf>,
    /// Upload every online and human score to Langfuse (`POST /api/public/scores`, with
    /// the run's trace id and a deterministic score id). Credentials and URL come from
    /// `LANGFUSE_PUBLIC_KEY`, `LANGFUSE_SECRET_KEY` and `LANGFUSE_BASE_URL`; use with
    /// `--otel-preset langfuse` so runs have trace ids.
    #[cfg(feature = "langfuse")]
    #[arg(long)]
    pub langfuse_scores: bool,
    #[cfg(feature = "jwt")]
    #[command(flatten)]
    pub user_auth: UserAuthArgs,
    /// Deployment this server belongs to, the namespace of a graphrt gateway
    /// [default: $GRAPHRT_DEPLOYMENT]. Webhook run ids then include it, derived exactly
    /// as the gateway derives them, so deployments sharing a store never collide.
    /// Required by `--trust-gateway`, which refuses signed requests for another one.
    #[arg(long, value_name = "NAME")]
    pub deployment: Option<String>,
    #[cfg(feature = "trust-gateway")]
    #[command(flatten)]
    pub gateway: GatewayTrustArgs,
    #[command(flatten)]
    pub services: ServiceArgs,
}

/// Conversations kept by `graphrt serve` (see docs/web.md).
#[cfg(feature = "conversations")]
#[derive(Debug, Args)]
pub struct ConversationArgs {
    /// Keep conversations on the server: a run that names a `conversation_id` gets
    /// the earlier turns as `sys.history` and its answer is added when it succeeds,
    /// so a browser does not hold the history. Stored in `--store` (in memory
    /// without one). A verified end user reaches only their own conversations.
    /// `sys.history` from clients keeps working as before [env: GRAPHRT_CONVERSATIONS].
    #[arg(long)]
    pub conversations: bool,
    /// Messages kept per conversation. When a conversation grows beyond it, the
    /// oldest turns are dropped (0 = unlimited).
    #[arg(long, value_name = "N", default_value_t = 200, requires = "conversations")]
    pub conversation_max_messages: usize,
    /// Conversations each end user may keep. A new one beyond it replaces the
    /// user's least recently used one (0 = unlimited). API-key callers are not limited.
    #[arg(long, value_name = "N", default_value_t = 50, requires = "conversations")]
    pub conversations_per_user: usize,
    /// Delete conversations not used for this many days (0 = keep them).
    #[arg(long, value_name = "DAYS", default_value_t = 30, requires = "conversations")]
    pub conversation_ttl_days: u64,
}

/// Messaging channels of `graphrt serve` (see docs/channels.md).
#[cfg(feature = "channels")]
#[derive(Debug, Default, Args)]
pub struct ChannelArgs {
    /// Let customers talk to a workflow on a messaging channel: `telegram=WORKFLOW`,
    /// `whatsapp=WORKFLOW`, `slack=WORKFLOW` or `teams=WORKFLOW`. Each customer gets a conversation of their own, the
    /// message is `sys.query` (or the start input `INPUT`) and the answer goes back
    /// on the channel. Credentials come from the environment or `--env-file`:
    /// `TELEGRAM_BOT_TOKEN`, or `WHATSAPP_ACCESS_TOKEN`, `WHATSAPP_PHONE_NUMBER_ID`,
    /// `WHATSAPP_APP_SECRET` and `WHATSAPP_VERIFY_TOKEN`, `SLACK_BOT_TOKEN` with
    /// `SLACK_APP_TOKEN` (Socket Mode) or `SLACK_SIGNING_SECRET`, or `TEAMS_APP_ID`,
    /// `TEAMS_APP_PASSWORD` and `TEAMS_TENANT_ID`. Turns on `--conversations`.
    /// Repeatable. Off unless given. See docs/channels.md.
    #[arg(long, value_name = "KIND=WORKFLOW[=INPUT]")]
    pub channel: Vec<String>,
    /// A channels file (YAML or JSON) for what the flag cannot say: several bots,
    /// allowed senders, group chats, approvers, the server's own sentences.
    #[arg(long, value_name = "FILE")]
    pub channels: Option<PathBuf>,
    /// Messages per minute one customer may send, with bursts of the same size
    /// (0 = unlimited). A channels file can set it per channel.
    #[arg(long, value_name = "N", default_value_t = crate::channels::config::DEFAULT_RATE_LIMIT)]
    pub channel_rate_limit: u32,
    /// Messages per minute all customers of one channel together may send
    /// (0 = unlimited). Anyone can write to a bot, so this is what bounds the
    /// model spend of a channel.
    #[arg(long, value_name = "N", default_value_t = crate::channels::config::DEFAULT_BUDGET)]
    pub channel_budget: u32,
    /// The language of what the server itself says on a channel (welcome, "something
    /// went wrong", "one moment please"...): `en` or `id`. A channels file can set
    /// `language` and single `messages` per channel.
    #[arg(long, value_name = "LANG", default_value = "en")]
    pub channel_language: String,
}

/// Monitoring a served agent from another device (see docs/monitoring.md).
#[cfg(feature = "devices")]
#[derive(Debug, Args)]
pub struct MonitorArgs {
    /// Serve the monitoring page at `/monitor/`: the runs, what each did, and the
    /// requests that wait for a person, for a phone or a desktop. It can be
    /// installed as an app. Turns on `--devices` and `--approvals`. Off unless given
    /// [env: GRAPHRT_MONITOR].
    #[cfg(feature = "web-monitor")]
    #[arg(long)]
    pub monitor: bool,
    /// Pair monitoring devices: `POST /v1/devices` (with the API key) gives a
    /// device its own token, which only reads runs and resolves approvals and
    /// can be revoked. Kept in `--store` (in memory without one), hashed
    /// [env: GRAPHRT_DEVICES].
    #[arg(long)]
    pub devices: bool,
    /// Serve the approvals inbox: `GET /v1/approvals` lists every run that waits
    /// for a person, `POST /v1/approvals/{run_id}` answers one [env: GRAPHRT_APPROVALS].
    #[cfg(feature = "approvals")]
    #[arg(long)]
    pub approvals: bool,
    /// Carry out `on_expire` of `human-input` requests whose time limit passed:
    /// answer with the named action, or stop the run. Without it a request waits
    /// until someone answers. Turns on `--approvals` [env: GRAPHRT_EXPIRE_APPROVALS].
    #[cfg(feature = "approvals")]
    #[arg(long)]
    pub expire_approvals: bool,
    /// Send a push notification to subscribed browsers when a run fails or waits
    /// for a person (Web Push). CONTACT is an address push services may write to
    /// about this server, e.g. `mailto:you@example.com`. Off unless given
    /// [env: GRAPHRT_PUSH].
    #[cfg(feature = "web-push")]
    #[arg(long, value_name = "CONTACT")]
    pub push: Option<String>,
    /// The address clients reach this server at, e.g. `https://agent.example.com`
    /// [env: GRAPHRT_PUBLIC_URL]. Used in the pairing link of a new device.
    /// Without it the link names the host of the pairing request.
    #[arg(long, value_name = "URL")]
    pub public_url: Option<String>,
}

/// Who may use the chat page of `graphrt serve` (`--chat-access`).
#[cfg(feature = "web-chat")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum ChatAccess {
    /// Holders of the API key: the page asks for it once and keeps it in the browser.
    #[default]
    Key,
    /// End users with a token the server verifies (`--verify-jwt`): the embedding
    /// page hands the token to the widget.
    Users,
    /// Anyone who opens the page: each browser gets a guest session, limited by
    /// `--chat-guest-rate-limit` and `--chat-guest-budget`.
    Guests,
}

/// The chat page and widget of `graphrt serve` (see docs/web.md).
#[cfg(feature = "web-chat")]
#[derive(Debug, Args)]
pub struct ChatArgs {
    /// Serve a chat page for this workflow at `/chat/WORKFLOW` (and `/chat` for the
    /// first one), plus the widget script `/chat/embed.js`. `WORKFLOW=INPUT` names
    /// the start input that takes the message; without it the message is
    /// `sys.query`. Repeatable [env: GRAPHRT_CHAT, commas]. Off unless given.
    #[arg(long, value_name = "WORKFLOW[=INPUT]")]
    pub chat: Vec<String>,
    /// Who may chat: `key`, `users` or `guests` [env: GRAPHRT_CHAT_ACCESS] [default: key].
    #[arg(long, value_enum, value_name = "WHO")]
    pub chat_access: Option<ChatAccess>,
    /// Title shown on the chat page and the widget [env: GRAPHRT_CHAT_TITLE]
    /// [default: the workflow name].
    #[arg(long, value_name = "TEXT")]
    pub chat_title: Option<String>,
    /// With `--chat-access guests`: runs per minute one guest may start, with bursts
    /// of the same size (0 = unlimited) [env: GRAPHRT_CHAT_GUEST_RATE_LIMIT] [default: 10].
    #[arg(long, value_name = "N")]
    pub chat_guest_rate_limit: Option<u32>,
    /// With `--chat-access guests`: runs per minute all guests together may start
    /// (0 = unlimited). A guest session costs nothing to open, so this is what
    /// bounds the model spend of a public page [env: GRAPHRT_CHAT_GUEST_BUDGET]
    /// [default: 120].
    #[arg(long, value_name = "N")]
    pub chat_guest_budget: Option<u32>,
    /// Draw `diagram` UI blocks as pictures in the chat: the path of a Mermaid
    /// build (`mermaid.min.js`), served at `/chat/mermaid.js` and loaded by the
    /// chat when a diagram first shows. Without it diagrams show as text.
    #[arg(long, value_name = "FILE")]
    pub chat_mermaid: Option<PathBuf>,
}

#[cfg(feature = "web-chat")]
impl ChatArgs {
    /// Runs per minute one guest may start without `--chat-guest-rate-limit`.
    pub const DEFAULT_GUEST_RATE_LIMIT: u32 = 10;
    /// Runs per minute all guests together may start without `--chat-guest-budget`.
    pub const DEFAULT_GUEST_BUDGET: u32 = 120;

    /// `--chat-access`, else `key`.
    pub fn access(&self) -> ChatAccess {
        self.chat_access.unwrap_or_default()
    }

    /// The chat option given without any `--chat`, if one was: they all need a chat page.
    pub fn option_without_chat(&self) -> Option<&'static str> {
        if !self.chat.is_empty() {
            return None;
        }
        [
            (self.chat_access.is_some(), "--chat-access"),
            (self.chat_title.is_some(), "--chat-title"),
            (self.chat_guest_rate_limit.is_some(), "--chat-guest-rate-limit"),
            (self.chat_guest_budget.is_some(), "--chat-guest-budget"),
            (self.chat_mermaid.is_some(), "--chat-mermaid"),
        ]
        .into_iter()
        .find_map(|(given, flag)| given.then_some(flag))
    }
}

/// Running behind a graphrt gateway (`graphrt serve --trust-gateway`, see
/// docs/gateway-protocol.md).
/// Gateway keys a standby agent received on stdin; never shown by `Debug`.
#[cfg(feature = "gateway")]
#[derive(Clone, Default)]
pub struct AssignedKeys(String);

#[cfg(feature = "gateway")]
impl AssignedKeys {
    /// Wraps the keys text (`kid:hex,...`).
    pub fn new(keys: String) -> Self {
        Self(keys)
    }

    /// The keys text.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

#[cfg(feature = "gateway")]
impl std::fmt::Debug for AssignedKeys {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AssignedKeys(<redacted>)")
    }
}

#[cfg(feature = "trust-gateway")]
#[derive(Debug, Default, Args)]
pub struct GatewayTrustArgs {
    /// Serve the gateway protocol under `/internal/v1`: requests signed with HMAC-SHA256
    /// by a key of `$GRAPHRT_GATEWAY_KEYS` (`kid:hexsecret[,kid:hexsecret]`, 32+ bytes
    /// each) for `--deployment`. Gateway-started runs survive a lost gateway connection
    /// for the reclaim grace instead of being aborted.
    #[arg(long)]
    pub trust_gateway: bool,
    /// Name of the environment variable holding the gateway keys.
    #[arg(long, value_name = "VAR", default_value = crate::gateway_agent::KEYS_ENV)]
    pub gateway_keys_env: String,
    /// Gateway keys handed over by a gateway's assignment of a standby agent
    /// (`graphrt agent-standby`, gateway builds only), instead of from the environment.
    #[cfg(feature = "gateway")]
    #[arg(skip)]
    pub assigned_keys: Option<AssignedKeys>,
    /// How long the outcome of a run whose gateway stream broke is kept for the gateway
    /// to reclaim before the run is marked failed (`owner_lost`).
    #[arg(long, value_name = "SECS", default_value_t = crate::gateway_agent::DEFAULT_RECLAIM_GRACE_SECS)]
    pub reclaim_grace_secs: u64,
}

/// End-user JWT verification for `graphrt serve` (see docs/serving.md).
#[cfg(feature = "jwt")]
#[derive(Debug, Default, Args)]
pub struct UserAuthArgs {
    /// Verify the JWT in this header-mapped start input (needs `--input-header INPUT=HEADER`).
    /// A request whose header holds a valid token may start runs without the API key; an
    /// invalid token is refused with 401. The verified `sub` becomes `sys.user_id`.
    #[arg(long, value_name = "INPUT")]
    pub verify_jwt: Option<String>,
    /// JWKS URL of the identity provider (ES256 / RS256 keys), e.g.
    /// `https://<project>.supabase.co/auth/v1/.well-known/jwks.json` [env: GRAPHRT_JWT_JWKS_URL].
    #[arg(long, value_name = "URL", requires = "verify_jwt")]
    pub jwt_jwks_url: Option<String>,
    /// Name of the environment variable holding an HS256 shared secret (e.g. the Supabase
    /// legacy JWT secret).
    #[arg(long, value_name = "VAR", requires = "verify_jwt")]
    pub jwt_secret_env: Option<String>,
    /// Required `iss` claim, e.g. `https://<project>.supabase.co/auth/v1` [env: GRAPHRT_JWT_ISSUER].
    #[arg(long, value_name = "ISS", requires = "verify_jwt")]
    pub jwt_issuer: Option<String>,
    /// Required `aud` claim (Supabase: `authenticated`) [env: GRAPHRT_JWT_AUDIENCE].
    #[arg(long, value_name = "AUD", requires = "verify_jwt")]
    pub jwt_audience: Option<String>,
    /// Runs per minute each verified user (`sub`) may start, with bursts of the same size;
    /// more get `429 Too Many Requests` with `Retry-After` (0 = unlimited).
    #[arg(long, value_name = "N", default_value_t = 0, requires = "verify_jwt")]
    pub user_rate_limit: u32,
    /// What a user-token request sees of its run: `answer` (default) streams only the
    /// answer text and the final result (outputs, answer, status, usage, pause prompts);
    /// `all` streams every engine event (node inputs and outputs, tool results, rendered
    /// prompts, the pause snapshot), as API-key callers get.
    #[arg(long, value_name = "SCOPE", value_enum, default_value_t = UserEvents::Answer, requires = "verify_jwt")]
    pub user_events: UserEvents,
}

/// `--user-events`: how much of a run a verified end user receives.
#[cfg(feature = "jwt")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum UserEvents {
    /// Answer chunks, the final result and pause prompts.
    #[default]
    Answer,
    /// Every engine event, as for API-key callers.
    All,
}
