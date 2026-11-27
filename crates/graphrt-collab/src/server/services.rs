//! Plugin services: server side work plugins do for the workspaces that enable them.
//!
//! A plugin declares services in its manifest (`services`, see
//! docs/plugin-manifest.md). The server loads the plugins of
//! `plugins.dirs` with the plugin host every runtime uses, so a service runs
//! sandboxed like any plugin: a process with a scrubbed environment (or an
//! HTTP service), per request timeouts, concurrency and message limits.
//!
//! - **Off until an owner enables it** for a workspace (`PUT .../services/<plugin>/<service>`),
//!   granting the workspace secrets it asks for, one by one. Secrets reach
//!   only plugins the server's operator trusts (`plugins.trusted`) and are
//!   sent as the tool call's secrets, never put in the environment.
//! - **Events**: `project.changed` (a few seconds after people stop typing),
//!   `version.saved`, `member.joined`. Project content goes along only to
//!   services that declare `reads_projects`.
//! - **Calls**: members with the service's `access` role call it
//!   (`POST .../services/<plugin>/<service>/call`) and get its answer.
//!
//! Every enable, disable and call is written to the workspace's audit log.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use graphrt_console::Role;
use graphrt_console::audit::{Actor, Event};
use graphrt_core::tool::{ToolCallContext, ToolRegistry};
use graphrt_plugin::{LoadedManifest, PluginHost, ServiceDescriptor};
use serde_json::{Value, json};

use super::Server;
use super::accounts::Account;
use super::config::PluginsConfig;
use super::error::{ServerError, ServerResult};
use super::util::now_ms;
use super::workspaces::{ServiceGrant, Workspace, WorkspaceFiles};

/// Quiet time after the last edit before `project.changed` is delivered.
const CHANGE_QUIET: Duration = Duration::from_secs(3);
/// The longest a service call may take, whatever the plugin's own limit says.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// The largest answer of a service call passed back, in bytes.
const MAX_ANSWER_BYTES: usize = 1024 * 1024;

/// A service a loaded plugin offers.
#[derive(Debug, Clone)]
struct Offered {
    plugin: String,
    trusted: bool,
    service: ServiceDescriptor,
}

impl Offered {
    fn key(&self) -> String {
        format!("{}/{}", self.plugin, self.service.id)
    }

    fn access(&self) -> Role {
        match self.service.access.as_str() {
            "viewer" => Role::Viewer,
            "owner" => Role::Owner,
            _ => Role::Operator,
        }
    }
}

/// The services of the server's plugins.
pub struct Services {
    offered: Vec<Offered>,
    tools: ToolRegistry,
    server: OnceLock<Weak<Server>>,
    /// Projects with a `project.changed` waiting: the time of the last edit and who made it.
    pending: Mutex<HashMap<(String, String), (Instant, String)>>,
}

impl std::fmt::Debug for Services {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys: Vec<String> = self.offered.iter().map(Offered::key).collect();
        f.debug_struct("Services").field("offered", &keys).finish_non_exhaustive()
    }
}

impl Services {
    /// Load the plugins of `config` that declare services.
    ///
    /// @throws ServerError::Config - When a plugin with services cannot be loaded.
    pub fn load(config: &PluginsConfig) -> ServerResult<Self> {
        let mut host = PluginHost::new().with_trusted(config.trusted.iter().cloned());
        let mut offered = Vec::new();
        for path in graphrt_plugin::discover(&config.dirs) {
            let plugin = LoadedManifest::from_path(&path)
                .map_err(|error| ServerError::Config(format!("plugin {}: {error}", path.display())))?;
            if plugin.manifest.services.is_empty() {
                continue;
            }
            let services = plugin.manifest.services.clone();
            let info = host.add(plugin).map_err(|error| ServerError::Config(format!("plugin: {error}")))?;
            for warning in &info.warnings {
                eprintln!("  plugin `{}`: {warning}", info.name);
            }
            for service in services {
                offered.push(Offered { plugin: info.name.clone(), trusted: info.trusted, service });
            }
        }
        let mut registry = graphrt_core::node::Registry::new();
        let mut tools = ToolRegistry::new();
        host.register(&mut registry, &mut tools).map_err(|error| ServerError::Config(format!("plugins: {error}")))?;
        Ok(Self { offered, tools, server: OnceLock::new(), pending: Mutex::new(HashMap::new()) })
    }

    /// Give the services the server they run in (once, after it is made).
    pub(crate) fn attach(&self, server: &Arc<Server>) {
        let _already = self.server.set(Arc::downgrade(server));
    }

    /// How many services are offered.
    pub fn len(&self) -> usize {
        self.offered.len()
    }

    /// Whether none are.
    pub fn is_empty(&self) -> bool {
        self.offered.is_empty()
    }

    fn find(&self, plugin: &str, service: &str) -> ServerResult<&Offered> {
        self.offered
            .iter()
            .find(|o| o.plugin == plugin && o.service.id == service)
            .ok_or_else(|| ServerError::NotFound("no such service on this server".into()))
    }

    /// The services offered, with what `workspace` granted each.
    pub fn list(&self, workspace: &Workspace) -> Vec<Value> {
        self.offered
            .iter()
            .map(|o| {
                let grant = workspace.services.get(&o.key());
                json!({
                    "plugin": o.plugin,
                    "id": o.service.id,
                    "title": o.service.title,
                    "description": o.service.description,
                    "access": o.service.access,
                    "events": o.service.events,
                    "asks_secrets": o.service.secrets,
                    "reads_projects": o.service.reads_projects,
                    "trusted": o.trusted,
                    "enabled": grant.is_some_and(|g| g.enabled),
                    "granted_secrets": grant.map(|g| g.secrets.clone()).unwrap_or_default(),
                })
            })
            .collect()
    }

    /// Enable or disable a service for `workspace`, granting `secrets`.
    ///
    /// @throws ServerError::BadRequest - For a secret the service does not ask for, or one
    ///   given to a plugin the server does not trust.
    #[allow(clippy::too_many_arguments)]
    pub fn grant(
        &self,
        server: &Server,
        workspace: &Workspace,
        plugin: &str,
        service: &str,
        enabled: bool,
        secrets: Vec<String>,
        by: &str,
    ) -> ServerResult<ServiceGrant> {
        let offered = self.find(plugin, service)?;
        if let Some(extra) = secrets.iter().find(|s| !offered.service.secrets.contains(s)) {
            return Err(ServerError::BadRequest(format!("the service does not ask for the secret `{extra}`")));
        }
        if !secrets.is_empty() && !offered.trusted {
            return Err(ServerError::BadRequest(
                "the server's operator does not trust this plugin with secrets (plugins.trusted)".into(),
            ));
        }
        let grant = ServiceGrant { enabled, secrets, updated_ms: now_ms(), updated_by: by.to_owned() };
        server.workspaces.set_service(&workspace.id, &offered.key(), grant.clone())?;
        Ok(grant)
    }

    /// Call a service as `account` (with `role` in `workspace`).
    ///
    /// @returns `{"text", "data", "is_error"}` of the plugin's answer.
    /// @throws ServerError - `NotFound` when not enabled, `Forbidden` below its access role,
    ///   `Internal` when the plugin fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn call(
        &self,
        server: &Server,
        workspace: &Workspace,
        role: Role,
        files: &WorkspaceFiles,
        account: &Account,
        plugin: &str,
        service: &str,
        input: Value,
    ) -> ServerResult<Value> {
        let offered = self.find(plugin, service)?;
        let grant = workspace
            .services
            .get(&offered.key())
            .filter(|g| g.enabled)
            .ok_or_else(|| ServerError::NotFound("this service is not enabled in the workspace".into()))?;
        let actor = Actor {
            kind: "user".into(),
            id: account.id.clone(),
            email: account.email.clone(),
            role: Some(role.name().into()),
        };
        if role < offered.access() {
            files.record(Event::ok(actor, "service.call").target(offered.key()).outcome("denied").status(403));
            return Err(ServerError::Forbidden(format!("this service needs the {} role", offered.access().name())));
        }
        let mut arguments = json!({
            "call": input,
            "workspace": { "id": workspace.id, "name": workspace.name },
            "caller": { "id": account.id, "name": account.name, "role": role },
        });
        if offered.service.reads_projects
            && let Some(project) = arguments.pointer("/call/project").and_then(Value::as_str).map(str::to_owned)
            && workspace.projects.contains_key(&project)
        {
            let content = project_json(server, workspace, &project, files)?;
            if let Some(object) = arguments.as_object_mut() {
                object.insert("project".into(), content);
            }
        }
        let secrets = granted_secrets(files, grant)?;
        let answer = self.invoke(offered, arguments, secrets).await;
        if let Err(error) = &answer {
            eprintln!("warning: collab: {error}");
        }
        let outcome = if answer.is_ok() { "ok" } else { "failed" };
        files.record(Event::ok(actor, "service.call").target(offered.key()).outcome(outcome));
        answer
    }

    async fn invoke(
        &self,
        offered: &Offered,
        arguments: Value,
        secrets: BTreeMap<String, String>,
    ) -> ServerResult<Value> {
        let runtime = self
            .tools
            .get(&offered.plugin)
            .ok_or_else(|| ServerError::Internal(format!("plugin `{}` has no tools", offered.plugin)))?;
        let context = ToolCallContext { secrets };
        let output = tokio::time::timeout(CALL_TIMEOUT, runtime.call(&offered.service.tool, arguments, &context))
            .await
            .map_err(|_| ServerError::Upstream(format!("the service {} took too long", offered.key())))?
            .map_err(|error| {
                let text: String = error.to_string().chars().take(400).collect();
                ServerError::Upstream(format!("the service {} failed: {text}", offered.key()))
            })?;
        let answer = json!({ "text": output.text, "data": output.data, "is_error": output.is_error });
        if serde_json::to_vec(&answer).map(|b| b.len()).unwrap_or(0) > MAX_ANSWER_BYTES {
            return Err(ServerError::Upstream(format!("the service {} answered too much", offered.key())));
        }
        Ok(answer)
    }

    /// A project changed: deliver `project.changed` once people stop editing for a moment.
    pub(crate) fn changed(&self, _server: &Server, workspace: &str, project: &str, by: &str) {
        if !self.offered.iter().any(|o| o.service.events.iter().any(|e| e == "project.changed")) {
            return;
        }
        let key = (workspace.to_owned(), project.to_owned());
        let first = {
            let mut pending = self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.insert(key.clone(), (Instant::now(), by.to_owned())).is_none()
        };
        if !first {
            return;
        }
        let Some(server) = self.server.get().cloned() else { return };
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(CHANGE_QUIET).await;
                let Some(server) = server.upgrade() else { return };
                let services = &server.services;
                let ready = {
                    let mut pending = services.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    match pending.get(&key) {
                        Some((at, _)) if at.elapsed() < CHANGE_QUIET => None,
                        Some(_) => pending.remove(&key).map(|(_, by)| by),
                        None => return,
                    }
                };
                if let Some(by) = ready {
                    services.emit(&server, &key.0, "project.changed", Some(&key.1), json!({ "by": by })).await;
                    return;
                }
            }
        });
    }

    /// Deliver `event` to the services `workspace` enabled that follow it.
    pub(crate) async fn emit(
        &self,
        server: &Server,
        workspace: &str,
        event: &str,
        project: Option<&str>,
        details: Value,
    ) {
        let Some(ws) = server.workspaces.get(workspace) else { return };
        let Ok(files) = server.workspaces.files(workspace) else { return };
        for offered in &self.offered {
            let Some(grant) = ws.services.get(&offered.key()).filter(|g| g.enabled) else { continue };
            if !offered.service.events.iter().any(|e| e == event) {
                continue;
            }
            let mut arguments = json!({
                "event": event,
                "workspace": { "id": ws.id, "name": ws.name },
                "details": details,
                "at_ms": now_ms(),
            });
            if let Some(project) = project.filter(|p| ws.projects.contains_key(*p)) {
                let content = if offered.service.reads_projects {
                    project_json(server, &ws, project, &files).unwrap_or(Value::Null)
                } else {
                    json!({ "id": project, "name": ws.projects.get(project).map(|p| p.name.clone()) })
                };
                if let Some(object) = arguments.as_object_mut() {
                    object.insert("project".into(), content);
                }
            }
            let outcome = match granted_secrets(&files, grant) {
                Ok(secrets) => match self.invoke(offered, arguments, secrets).await {
                    Ok(answer) if answer.get("is_error") == Some(&Value::Bool(true)) => "failed",
                    Ok(_) => "ok",
                    Err(error) => {
                        eprintln!("warning: collab: {error}");
                        "failed"
                    }
                },
                Err(_) => "failed",
            };
            files.record(
                Event::ok(Actor::system(), "service.event")
                    .target(offered.key())
                    .outcome(outcome)
                    .details(json!({ "event": event, "project": project })),
            );
        }
    }
}

fn project_json(server: &Server, workspace: &Workspace, project: &str, files: &WorkspaceFiles) -> ServerResult<Value> {
    let room = server.rooms.get_or_open(&workspace.id, project, files.project_dir(project))?;
    let content = room.json();
    if room.peers() == 0 {
        server.rooms.close_when_idle(&room);
    }
    Ok(json!({
        "id": project,
        "name": workspace.projects.get(project).map(|p| p.name.clone()),
        "content": content,
    }))
}

fn granted_secrets(files: &WorkspaceFiles, grant: &ServiceGrant) -> ServerResult<BTreeMap<String, String>> {
    if grant.secrets.is_empty() {
        return Ok(BTreeMap::new());
    }
    let values = files.secrets.values()?;
    Ok(values.into_iter().filter(|(name, _)| grant.secrets.contains(name)).collect())
}

impl Server {
    /// Deliver a workspace event to services, in the background.
    pub(crate) fn service_event(
        self: &Arc<Self>,
        workspace: &str,
        event: &'static str,
        project: Option<&str>,
        details: Value,
    ) {
        if self.services.is_empty() {
            return;
        }
        let server = Arc::clone(self);
        let workspace = workspace.to_owned();
        let project = project.map(str::to_owned);
        tokio::spawn(async move {
            server.services.emit(&server, &workspace, event, project.as_deref(), details).await;
        });
    }
}
