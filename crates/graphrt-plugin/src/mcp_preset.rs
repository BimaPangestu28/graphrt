//! `runtime: mcp` plugins (MCP server presets) and plugin templates.
//!
//! An MCP preset plugin ships no code: it declares a remote MCP server (an
//! official Upwork, Notion or Linear server, ...) that a host registers as an
//! MCP tool provider when the plugin is installed. Its tools come from the
//! server itself, so the manifest has no `nodes` or `tools`.
//!
//! ```json
//! {
//!   "name": "upwork",
//!   "version": "1.0.0",
//!   "runtime": "mcp",
//!   "mcp": {
//!     "url": "https://mcp.upwork.com/mcp",
//!     "transport": "streamable-http",
//!     "auth": "oauth",
//!     "approval": { "default": "ask", "read_only": ["find_*", "get_*", "list_*", "search_*"] },
//!     "guide": "guide.md"
//!   },
//!   "templates": [{ "id": "morning-jobs", "title": "…", "description": "…", "prompt": "…" }]
//! }
//! ```
//!
//! `templates` (any runtime) are starter prompts a host can offer on its
//! "new agent" screen while the plugin is installed.

use serde::{Deserialize, Serialize};

/// Longest `guide` file a host should read (bytes).
pub const MAX_GUIDE_BYTES: u64 = 32 * 1024;
/// Most templates per plugin.
pub const MAX_TEMPLATES: usize = 20;
const MAX_TEMPLATE_TITLE_CHARS: usize = 120;
const MAX_TEMPLATE_DESCRIPTION_CHARS: usize = 400;
const MAX_TEMPLATE_PROMPT_CHARS: usize = 4_000;
const MAX_PATTERN_CHARS: usize = 128;

/// The remote MCP server an `mcp` plugin declares.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerSpec {
    /// Streamable HTTP endpoint, `https://` (or `http://` to the local machine).
    pub url: String,
    #[serde(default)]
    pub transport: McpTransportKind,
    #[serde(default)]
    pub auth: McpAuth,
    /// With `auth: bearer`: the host variable / secret holding the token. It must
    /// also be listed in `permissions.env`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token_env: Option<String>,
    #[serde(default)]
    pub approval: McpApproval,
    /// Markdown file (relative to the plugin directory) with guidance for the
    /// agent builder; `{server}` stands for the server name the host uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guide: Option<String>,
}

/// How the host talks to the server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpTransportKind {
    /// MCP Streamable HTTP (the only one for now).
    #[default]
    StreamableHttp,
}

/// How the host authenticates to the server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpAuth {
    #[default]
    None,
    /// A token from `bearer_token_env`.
    Bearer,
    /// OAuth sign-in following the MCP authorization spec (the host runs it).
    Oauth,
}

/// Which tools a host runs without asking the user first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpApproval {
    #[serde(default)]
    pub default: McpApprovalDefault,
    /// Tools that only read: exact names or `prefix*` patterns. A host should
    /// still ask for a matching tool whose name says it sends.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_only: Vec<String>,
    /// Tools that always ask first, whatever their name (patterns as above).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ask_first: Vec<String>,
}

/// What happens to a tool that no pattern matches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpApprovalDefault {
    /// It asks first (the safe default).
    #[default]
    Ask,
    /// The host's own rule decides from the tool name (`get_…` reads, `send_…` sends).
    ByName,
}

impl McpApproval {
    /// Whether `tool` matches one of `read_only` (and none of `ask_first`).
    pub fn is_declared_read_only(&self, tool: &str) -> bool {
        !self.ask_first.iter().any(|pattern| matches_pattern(pattern, tool))
            && self.read_only.iter().any(|pattern| matches_pattern(pattern, tool))
    }

    /// Whether `tool` matches one of `ask_first`.
    pub fn is_declared_ask_first(&self, tool: &str) -> bool {
        self.ask_first.iter().any(|pattern| matches_pattern(pattern, tool))
    }
}

/// `name` matches an exact pattern, or a `prefix*` pattern.
pub fn matches_pattern(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

/// A starter prompt a plugin offers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginTemplate {
    /// `[a-z0-9-]`, unique in the plugin.
    pub id: String,
    pub title: String,
    pub description: String,
    /// The text put into the "describe the job" box.
    pub prompt: String,
}

impl McpServerSpec {
    /// Checks the server spec (`permission_env`: the manifest's `permissions.env`).
    pub(crate) fn validate(&self, permission_env: &[String]) -> Result<(), String> {
        let url = self.url.trim();
        let is_https = url.starts_with("https://");
        if !(is_https || (url.starts_with("http://") && crate::manifest::is_loopback_url(url))) {
            return Err(format!("`mcp.url` must be an https:// URL (http:// only to this machine), got `{url}`"));
        }
        if url.contains(char::is_whitespace) {
            return Err("`mcp.url` must not contain spaces".into());
        }
        match (self.auth, &self.bearer_token_env) {
            (McpAuth::Bearer, None) => return Err("`mcp.auth: bearer` requires `mcp.bearer_token_env`".into()),
            (McpAuth::Bearer, Some(var)) if !permission_env.iter().any(|e| e == var) => {
                return Err(format!("`mcp.bearer_token_env` `{var}` must also be listed in `permissions.env`"));
            }
            (McpAuth::None | McpAuth::Oauth, Some(_)) => {
                return Err("`mcp.bearer_token_env` only applies to `mcp.auth: bearer`".into());
            }
            _ => {}
        }
        for pattern in self.approval.read_only.iter().chain(&self.approval.ask_first) {
            validate_pattern(pattern)?;
        }
        let bad_guide =
            self.guide.as_ref().filter(|guide| !crate::manifest::is_inside(guide) || !guide.ends_with(".md"));
        if let Some(guide) = bad_guide {
            return Err(format!("`mcp.guide` `{guide}` must be a relative `.md` file inside the plugin directory"));
        }
        Ok(())
    }
}

fn validate_pattern(pattern: &str) -> Result<(), String> {
    let body = pattern.strip_suffix('*').unwrap_or(pattern);
    let valid = !body.is_empty()
        && pattern.len() <= MAX_PATTERN_CHARS
        && body.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if valid {
        Ok(())
    } else {
        Err(format!(
            "approval pattern `{}` must be a tool name or `prefix*` (letters, digits, `_`, `-`, `.`)",
            pattern.escape_debug()
        ))
    }
}

/// Checks the templates of a manifest.
pub(crate) fn validate_templates(templates: &[PluginTemplate]) -> Result<(), String> {
    if templates.len() > MAX_TEMPLATES {
        return Err(format!("at most {MAX_TEMPLATES} templates"));
    }
    let mut ids = std::collections::HashSet::new();
    for template in templates {
        let id_ok = !template.id.is_empty()
            && template.id.len() <= 64
            && template.id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !id_ok || !ids.insert(template.id.as_str()) {
            return Err(format!("invalid or duplicate template id `{}`", template.id.escape_debug()));
        }
        let fields = [
            ("title", &template.title, MAX_TEMPLATE_TITLE_CHARS),
            ("description", &template.description, MAX_TEMPLATE_DESCRIPTION_CHARS),
            ("prompt", &template.prompt, MAX_TEMPLATE_PROMPT_CHARS),
        ];
        for (field, text, max) in fields {
            if text.trim().is_empty() || text.chars().count() > max {
                return Err(format!("template `{}`: `{field}` must be 1 to {max} characters", template.id));
            }
            if text.chars().any(|c| c.is_control() && c != '\n') {
                return Err(format!("template `{}`: `{field}` contains a control character", template.id));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_match_exact_names_and_prefixes() {
        assert!(matches_pattern("find_*", "find_jobs"));
        assert!(!matches_pattern("find_*", "refind_jobs"));
        assert!(matches_pattern("list_accounts", "list_accounts"));
        assert!(!matches_pattern("list_accounts", "list_accounts_all"));
        let approval = McpApproval {
            default: McpApprovalDefault::Ask,
            read_only: vec!["get_*".into()],
            ask_first: vec!["get_secret".into()],
        };
        assert!(approval.is_declared_read_only("get_job"));
        assert!(!approval.is_declared_read_only("get_secret"), "ask_first wins");
        assert!(approval.is_declared_ask_first("get_secret"));
    }

    #[test]
    fn rejects_bad_patterns() {
        for bad in ["*", "", "a b*", "x*y", "wild**"] {
            assert!(validate_pattern(bad).is_err(), "{bad}");
        }
        assert!(validate_pattern("search_*").is_ok());
    }
}
