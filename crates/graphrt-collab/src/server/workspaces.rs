//! Team workspaces: members and their roles, invites, projects and the
//! plugin services a workspace enabled.
//!
//! Roles are the console's ([`Role`]):
//!
//! | Role | In a workspace |
//! | --- | --- |
//! | `viewer` | open projects read only, see who is there, read versions |
//! | `operator` ("can edit") | what a viewer may, and edit projects, create projects, save and restore versions, call services |
//! | `owner` | everything: members, roles, invites, services, secrets, the audit log, deleting projects and the workspace |
//!
//! Each workspace is a directory `workspaces/<id>/` with `workspace.json`,
//! its audit log (`audit/`), its secrets (`secrets.json`) and its projects
//! (`projects/<id>/`).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use graphrt_console::Role;
use graphrt_console::audit::{AuditLog, Event};
use graphrt_console::secrets::{SecretKey, Secrets};
use serde::{Deserialize, Serialize};

use super::error::{ServerError, ServerResult};
use super::util::{
    MS_PER_DAY, constant_time_eq, ensure_dir, is_id, now_ms, random_id, random_token, read_json, sha256_hex, write_json,
};

const INVITE_PREFIX: &str = "gci1.";
/// The longest name of a workspace or project.
pub const MAX_NAME_CHARS: usize = 120;
/// Open invites per workspace.
const MAX_INVITES: usize = 100;

/// A member of a workspace.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Member {
    pub role: Role,
    pub added_ms: u64,
    /// The account that added them (by invite or by hand).
    #[serde(default)]
    pub added_by: String,
}

/// An invite: a code that adds whoever redeems it with a role.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Invite {
    pub id: String,
    /// SHA-256 of the code. The code itself is shown once.
    hash: String,
    pub role: Role,
    /// Only the account with this verified email may redeem it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub max_uses: u32,
    #[serde(default)]
    pub uses: u32,
    pub expires_ms: u64,
    pub created_ms: u64,
    pub created_by: String,
}

impl Invite {
    /// Whether `hash` is the hash of this invite's code.
    pub fn matches(&self, hash: &str) -> bool {
        constant_time_eq(self.hash.as_bytes(), hash.as_bytes())
    }

    /// Whether it can still be redeemed at `now_ms`.
    pub fn usable(&self, now_ms: u64) -> bool {
        self.expires_ms > now_ms && self.uses < self.max_uses
    }

    /// What the owners see of it.
    pub fn public(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "role": self.role,
            "email": self.email,
            "max_uses": self.max_uses,
            "uses": self.uses,
            "expires_ms": self.expires_ms,
            "created_ms": self.created_ms,
            "created_by": self.created_by,
        })
    }
}

/// A project of a workspace (its document lives in `projects/<id>/`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub name: String,
    pub created_ms: u64,
    pub created_by: String,
    #[serde(default)]
    pub updated_ms: u64,
}

/// What a workspace granted a plugin service.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServiceGrant {
    pub enabled: bool,
    /// Workspace secrets the service receives.
    #[serde(default)]
    pub secrets: Vec<String>,
    pub updated_ms: u64,
    pub updated_by: String,
}

/// A workspace, as kept in `workspace.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    pub created_ms: u64,
    pub created_by: String,
    #[serde(default)]
    pub members: BTreeMap<String, Member>,
    #[serde(default)]
    pub invites: Vec<Invite>,
    #[serde(default)]
    pub projects: BTreeMap<String, ProjectInfo>,
    /// `<plugin>/<service>` -> grant.
    #[serde(default)]
    pub services: BTreeMap<String, ServiceGrant>,
}

impl Workspace {
    /// The role of `account`, `None` when not a member.
    pub fn role_of(&self, account: &str) -> Option<Role> {
        self.members.get(account).map(|m| m.role)
    }

    fn owners(&self) -> usize {
        self.members.values().filter(|m| m.role == Role::Owner).count()
    }
}

/// The open files of a workspace.
#[derive(Debug)]
pub struct WorkspaceFiles {
    pub dir: PathBuf,
    pub audit: AuditLog,
    pub secrets: Secrets,
}

impl WorkspaceFiles {
    /// The directory of project `project`.
    pub fn project_dir(&self, project: &str) -> PathBuf {
        self.dir.join("projects").join(project)
    }

    /// Write `event` to the workspace's audit log. A failed write goes to stderr
    /// (what it records has happened already).
    pub fn record(&self, event: Event) {
        if let Err(error) = self.audit.append(event) {
            eprintln!("warning: collab: cannot write the audit log: {error}");
        }
    }
}

/// Every workspace.
#[derive(Debug)]
pub struct Workspaces {
    dir: PathBuf,
    retention_days: u64,
    secret_key: Option<String>,
    inner: RwLock<HashMap<String, Workspace>>,
    files: RwLock<HashMap<String, Arc<WorkspaceFiles>>>,
    /// The modification time of each `workspace.json` as last read or written:
    /// a file changed by another process (`graphrt collab invite` next to a
    /// running server) is read again before it is used.
    stamps: std::sync::Mutex<HashMap<String, Stamp>>,
}

impl Workspaces {
    /// The workspaces in `data_dir/workspaces`.
    ///
    /// @param secret_key - The key text of workspace secrets (32 bytes, base64 or hex).
    /// @throws ServerError - When the directory or a workspace file cannot be read, or the key is unusable.
    pub fn open(data_dir: &Path, retention_days: u64, secret_key: Option<String>) -> ServerResult<Self> {
        if let Some(key) = &secret_key {
            SecretKey::parse(key).map_err(|error| ServerError::Config(format!("secrets key: {error}")))?;
        }
        let dir = data_dir.join("workspaces");
        ensure_dir(&dir)?;
        let mut inner = HashMap::new();
        let mut stamps = HashMap::new();
        let entries = std::fs::read_dir(&dir)
            .map_err(|error| ServerError::Io(format!("cannot read {}: {error}", dir.display())))?;
        for entry in entries.flatten() {
            let path = entry.path().join("workspace.json");
            let stamp = modified(&path);
            if let Some(workspace) = read_json::<Workspace>(&path)? {
                if let Some(stamp) = stamp {
                    stamps.insert(workspace.id.clone(), stamp);
                }
                inner.insert(workspace.id.clone(), workspace);
            }
        }
        Ok(Self {
            dir,
            retention_days,
            secret_key,
            inner: RwLock::new(inner),
            files: RwLock::new(HashMap::new()),
            stamps: std::sync::Mutex::new(stamps),
        })
    }

    fn file_of(&self, id: &str) -> PathBuf {
        self.dir.join(id).join("workspace.json")
    }

    /// Read workspace `id` again when its file changed since this process last read or wrote it.
    fn refresh(&self, id: &str) {
        if !is_id(id, "ws_") {
            return;
        }
        let path = self.file_of(id);
        let Some(stamp) = modified(&path) else { return };
        let mut stamps = self.stamps.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if stamps.get(id) == Some(&stamp) {
            return;
        }
        match read_json::<Workspace>(&path) {
            Ok(Some(workspace)) if workspace.id == id => {
                stamps.insert(id.to_owned(), stamp);
                self.inner.write().unwrap_or_else(std::sync::PoisonError::into_inner).insert(id.to_owned(), workspace);
            }
            Ok(_) => {}
            Err(error) => eprintln!("warning: collab: {error}"),
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, Workspace>> {
        self.inner.read().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn save(&self, workspace: &Workspace) -> ServerResult<()> {
        let dir = self.dir.join(&workspace.id);
        ensure_dir(&dir)?;
        let path = dir.join("workspace.json");
        write_json(&path, workspace)?;
        if let Some(stamp) = modified(&path) {
            self.stamps.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(workspace.id.clone(), stamp);
        }
        Ok(())
    }

    /// Change workspace `id` with `change`, then save it.
    fn update<R>(&self, id: &str, change: impl FnOnce(&mut Workspace) -> ServerResult<R>) -> ServerResult<R> {
        self.refresh(id);
        let mut inner = self.inner.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        let workspace = inner.get_mut(id).ok_or_else(not_found)?;
        let mut changed = workspace.clone();
        let result = change(&mut changed)?;
        self.save(&changed)?;
        *workspace = changed;
        Ok(result)
    }

    /// Workspace `id`.
    pub fn get(&self, id: &str) -> Option<Workspace> {
        self.refresh(id);
        self.read().get(id).cloned()
    }

    /// The workspace `id` and the role `account` has in it.
    ///
    /// @throws ServerError::NotFound - When there is none or `account` is not a member
    ///   (the same answer, so outsiders learn nothing).
    pub fn membership(&self, id: &str, account: &str) -> ServerResult<(Workspace, Role)> {
        let workspace = self.get(id).ok_or_else(not_found)?;
        let role = workspace.role_of(account).ok_or_else(not_found)?;
        Ok((workspace, role))
    }

    /// The workspaces `account` is a member of.
    pub fn of_account(&self, account: &str) -> Vec<(Workspace, Role)> {
        let mut list: Vec<(Workspace, Role)> =
            self.read().values().filter_map(|w| w.role_of(account).map(|role| (w.clone(), role))).collect();
        list.sort_by(|(a, _), (b, _)| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        list
    }

    /// The number of workspaces `account` created.
    pub fn created_by(&self, account: &str) -> usize {
        self.read().values().filter(|w| w.created_by == account).count()
    }

    /// The open files (audit log, secrets) of workspace `id`.
    ///
    /// @throws ServerError - When they cannot be opened.
    pub fn files(&self, id: &str) -> ServerResult<Arc<WorkspaceFiles>> {
        if let Some(files) = self.files.read().unwrap_or_else(std::sync::PoisonError::into_inner).get(id) {
            return Ok(Arc::clone(files));
        }
        if !is_id(id, "ws_") || self.get(id).is_none() {
            return Err(not_found());
        }
        let dir = self.dir.join(id);
        ensure_dir(&dir)?;
        let key = self.secret_key.as_deref().map(SecretKey::parse).transpose()?;
        let files = Arc::new(WorkspaceFiles {
            audit: AuditLog::open(&dir, self.retention_days)?,
            secrets: Secrets::open(&dir, key)?,
            dir,
        });
        let mut cache = self.files.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(Arc::clone(cache.entry(id.to_owned()).or_insert(files)))
    }

    /// Create a workspace owned by `owner`.
    ///
    /// @throws ServerError - `BadRequest` for an empty or long name, `Io`.
    pub fn create(&self, name: &str, owner: &str) -> ServerResult<Workspace> {
        let name = clean_name(name)?;
        let now = now_ms();
        let workspace = Workspace {
            id: random_id("ws_")?,
            name,
            created_ms: now,
            created_by: owner.to_owned(),
            members: BTreeMap::from([(
                owner.to_owned(),
                Member { role: Role::Owner, added_ms: now, added_by: owner.to_owned() },
            )]),
            invites: Vec::new(),
            projects: BTreeMap::new(),
            services: BTreeMap::new(),
        };
        self.save(&workspace)?;
        self.inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(workspace.id.clone(), workspace.clone());
        Ok(workspace)
    }

    /// Create a workspace with no member yet: its first owner joins with an
    /// owner invite (the bootstrap of a new server).
    pub fn create_unowned(&self, name: &str) -> ServerResult<String> {
        let workspace = self.create(name, "")?;
        self.update(&workspace.id, |w| {
            w.members.clear();
            w.created_by = "system".into();
            Ok(())
        })?;
        Ok(workspace.id)
    }

    /// The id of the first workspace named `name`.
    pub fn by_name(&self, name: &str) -> Option<String> {
        let mut matches: Vec<&Workspace> = Vec::new();
        let inner = self.read();
        matches.extend(inner.values().filter(|w| w.name == name.trim()));
        matches.sort_by_key(|w| w.created_ms);
        matches.first().map(|w| w.id.clone())
    }

    /// Every workspace id.
    pub fn ids(&self) -> Vec<String> {
        self.read().keys().cloned().collect()
    }

    /// The number of workspaces.
    pub fn of_all(&self) -> usize {
        self.read().len()
    }

    /// Rename workspace `id`.
    pub fn rename(&self, id: &str, name: &str) -> ServerResult<()> {
        let name = clean_name(name)?;
        self.update(id, |w| {
            w.name = name;
            Ok(())
        })
    }

    /// Delete workspace `id` and everything in it.
    pub fn delete(&self, id: &str) -> ServerResult<()> {
        if self.inner.write().unwrap_or_else(std::sync::PoisonError::into_inner).remove(id).is_none() {
            return Err(not_found());
        }
        self.files.write().unwrap_or_else(std::sync::PoisonError::into_inner).remove(id);
        self.stamps.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(id);
        let dir = self.dir.join(id);
        std::fs::remove_dir_all(&dir)
            .map_err(|error| ServerError::Io(format!("cannot remove {}: {error}", dir.display())))
    }

    /// Give `account` role `role` (adding them when they are not a member).
    ///
    /// @throws ServerError::Conflict - When it would leave the workspace without an owner,
    ///   `Limit` when the workspace is full.
    pub fn set_role(&self, id: &str, account: &str, role: Role, by: &str, max_members: usize) -> ServerResult<()> {
        self.update(id, |w| {
            let other_owners = w.owners_after_demotion();
            match w.members.get_mut(account) {
                Some(member) => {
                    if member.role == Role::Owner && role != Role::Owner && other_owners == 0 {
                        return Err(ServerError::Conflict("a workspace keeps at least one owner".into()));
                    }
                    member.role = role;
                }
                None => {
                    if w.members.len() >= max_members {
                        return Err(ServerError::Limit(format!("a workspace has at most {max_members} members")));
                    }
                    w.members.insert(account.to_owned(), Member { role, added_ms: now_ms(), added_by: by.to_owned() });
                }
            }
            Ok(())
        })
    }

    /// Remove `account` from workspace `id`.
    ///
    /// @throws ServerError - `NotFound` when not a member, `Conflict` for the last owner.
    pub fn remove_member(&self, id: &str, account: &str) -> ServerResult<()> {
        self.update(id, |w| {
            let member = w.members.get(account).ok_or_else(|| ServerError::NotFound("not a member".into()))?;
            if member.role == Role::Owner && w.owners() == 1 {
                return Err(ServerError::Conflict(
                    "the last owner cannot leave: make someone else an owner first, or delete the workspace".into(),
                ));
            }
            w.members.remove(account);
            Ok(())
        })
    }

    /// Create an invite to workspace `id`.
    ///
    /// @returns The invite and its code (shown once).
    /// @throws ServerError::BadRequest - For an owner invite with many uses, a long expiry or too many open invites.
    pub fn invite(
        &self,
        id: &str,
        role: Role,
        email: Option<String>,
        max_uses: u32,
        expires_hours: u64,
        by: &str,
    ) -> ServerResult<(Invite, String)> {
        if max_uses == 0 || max_uses > 1_000 {
            return Err(ServerError::BadRequest("max_uses is 1 to 1000".into()));
        }
        if role == Role::Owner && max_uses != 1 {
            return Err(ServerError::BadRequest("an owner invite can be used once".into()));
        }
        if expires_hours == 0 || expires_hours > 30 * 24 {
            return Err(ServerError::BadRequest("an invite expires within 1 hour to 30 days".into()));
        }
        let email = email.map(|e| e.trim().to_ascii_lowercase()).filter(|e| !e.is_empty());
        if email.as_deref().is_some_and(|e| !e.contains('@')) {
            return Err(ServerError::BadRequest("email is not an email address".into()));
        }
        let code = format!("{INVITE_PREFIX}{id}.{}", random_token()?);
        let now = now_ms();
        let invite = Invite {
            id: random_id("inv_")?,
            hash: sha256_hex(code.as_bytes()),
            role,
            email,
            max_uses,
            uses: 0,
            expires_ms: now + expires_hours * 60 * 60 * 1000,
            created_ms: now,
            created_by: by.to_owned(),
        };
        self.update(id, |w| {
            w.invites.retain(|i| i.expires_ms > now && i.uses < i.max_uses);
            if w.invites.len() >= MAX_INVITES {
                return Err(ServerError::Limit(format!("a workspace has at most {MAX_INVITES} open invites")));
            }
            w.invites.push(invite.clone());
            Ok(())
        })?;
        Ok((invite, code))
    }

    /// Revoke invite `invite` of workspace `id`.
    pub fn revoke_invite(&self, id: &str, invite: &str) -> ServerResult<()> {
        self.update(id, |w| {
            let before = w.invites.len();
            w.invites.retain(|i| i.id != invite);
            if w.invites.len() == before {
                return Err(ServerError::NotFound("no such invite".into()));
            }
            Ok(())
        })
    }

    /// The workspace an invite code is for (without redeeming it).
    pub fn invite_target(code: &str) -> Option<&str> {
        let rest = code.trim().strip_prefix(INVITE_PREFIX)?;
        let (workspace, _) = rest.split_once('.')?;
        is_id(workspace, "ws_").then_some(workspace)
    }

    /// Redeem invite `code` for `account` (with verified `email`, when it has one).
    ///
    /// @returns The workspace and the role `account` now has. A member keeps
    ///   their role when it is higher than the invite's.
    /// @throws ServerError::NotFound - For an unknown, used up, expired code or another email.
    pub fn redeem(
        &self,
        code: &str,
        account: &str,
        email: Option<&str>,
        max_members: usize,
    ) -> ServerResult<(Workspace, Role)> {
        let invalid = || ServerError::NotFound("this invite is not valid (used, expired or revoked)".into());
        let code = code.trim();
        let id = Self::invite_target(code).ok_or_else(invalid)?.to_owned();
        let hash = sha256_hex(code.as_bytes());
        let now = now_ms();
        self.update(&id, |w| {
            let invite = w
                .invites
                .iter_mut()
                .find(|i| constant_time_eq(i.hash.as_bytes(), hash.as_bytes()))
                .ok_or_else(invalid)?;
            if invite.expires_ms <= now || invite.uses >= invite.max_uses {
                return Err(invalid());
            }
            if let Some(wanted) = &invite.email
                && email.is_none_or(|email| !email.eq_ignore_ascii_case(wanted))
            {
                return Err(ServerError::Forbidden("this invite is for another email address".into()));
            }
            let role = invite.role;
            let by = invite.created_by.clone();
            let is_member = w.members.contains_key(account);
            if !is_member && w.members.len() >= max_members {
                return Err(ServerError::Limit(format!("the workspace has at most {max_members} members")));
            }
            invite.uses += 1;
            let member = w.members.entry(account.to_owned()).or_insert(Member { role, added_ms: now, added_by: by });
            if member.role < role {
                member.role = role;
            }
            let role = member.role;
            Ok((w.clone(), role))
        })
    }

    /// Add project `name` to workspace `id`.
    ///
    /// @returns The project id.
    /// @throws ServerError::Limit - When the workspace has `max` projects.
    pub fn add_project(&self, id: &str, name: &str, by: &str, max: usize) -> ServerResult<String> {
        let name = clean_name(name)?;
        let project = random_id("pr_")?;
        self.update(id, |w| {
            if w.projects.len() >= max {
                return Err(ServerError::Limit(format!("a workspace has at most {max} projects")));
            }
            let now = now_ms();
            w.projects.insert(
                project.clone(),
                ProjectInfo { name, created_ms: now, created_by: by.to_owned(), updated_ms: now },
            );
            Ok(())
        })?;
        Ok(project)
    }

    /// Rename project `project`.
    pub fn rename_project(&self, id: &str, project: &str, name: &str) -> ServerResult<()> {
        let name = clean_name(name)?;
        self.update(id, |w| {
            let info = w.projects.get_mut(project).ok_or_else(|| ServerError::NotFound("no such project".into()))?;
            info.name = name;
            Ok(())
        })
    }

    /// Note that project `project` changed (shown in listings). Not saved at once.
    pub fn touch_project(&self, id: &str, project: &str) {
        let mut inner = self.inner.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(info) = inner.get_mut(id).and_then(|w| w.projects.get_mut(project)) {
            info.updated_ms = now_ms();
        }
    }

    /// Save every workspace (the `updated_ms` of projects), at shutdown.
    pub fn flush(&self) {
        for workspace in self.read().values() {
            if let Err(error) = self.save(workspace) {
                eprintln!("warning: collab: {error}");
            }
        }
    }

    /// Remove project `project` (the caller removes its files).
    pub fn remove_project(&self, id: &str, project: &str) -> ServerResult<()> {
        self.update(id, |w| {
            w.projects.remove(project).map(|_| ()).ok_or_else(|| ServerError::NotFound("no such project".into()))
        })
    }

    /// Enable or disable a plugin service in workspace `id`.
    pub fn set_service(&self, id: &str, key: &str, grant: ServiceGrant) -> ServerResult<()> {
        self.update(id, |w| {
            w.services.insert(key.to_owned(), grant);
            Ok(())
        })
    }
}

impl Workspace {
    fn owners_after_demotion(&self) -> usize {
        self.owners().saturating_sub(1)
    }
}

/// What tells a changed file: its modification time and length.
type Stamp = (std::time::SystemTime, u64);

fn modified(path: &Path) -> Option<Stamp> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

fn not_found() -> ServerError {
    ServerError::NotFound("no such workspace".into())
}

/// A name, trimmed, with no control characters, 1 to [`MAX_NAME_CHARS`] characters.
///
/// @throws ServerError::BadRequest - Otherwise.
pub fn clean_name(name: &str) -> ServerResult<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS || name.chars().any(char::is_control) {
        return Err(ServerError::BadRequest(format!(
            "a name has 1 to {MAX_NAME_CHARS} characters and no control characters"
        )));
    }
    Ok(name.to_owned())
}

/// Hours an invite lasts unless its maker says otherwise.
pub const DEFAULT_INVITE_HOURS: u64 = 7 * 24;
const _: () = assert!(DEFAULT_INVITE_HOURS * 60 * 60 * 1000 < 31 * MS_PER_DAY);

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("graphrt-collab-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn create_invite_redeem() {
        let dir = temp_dir("ws-invite");
        let all = Workspaces::open(&dir, 30, None).unwrap();
        let ws = all.create("Team", "acct_owner").unwrap();
        let (invite, code) = all.invite(&ws.id, Role::Operator, None, 2, 24, "acct_owner").unwrap();
        assert_eq!(invite.uses, 0);
        let (_, role) = all.redeem(&code, "acct_a", None, 100).unwrap();
        assert_eq!(role, Role::Operator);
        all.redeem(&code, "acct_b", None, 100).unwrap();
        // Used up.
        assert!(matches!(all.redeem(&code, "acct_c", None, 100), Err(ServerError::NotFound(_))));
        assert_eq!(all.membership(&ws.id, "acct_a").unwrap().1, Role::Operator);
        assert!(all.membership(&ws.id, "acct_c").is_err());
        let reopened = Workspaces::open(&dir, 30, None).unwrap();
        assert_eq!(reopened.get(&ws.id).unwrap().members.len(), 3);
    }

    #[test]
    fn invites_check_email_expiry_and_tampering() {
        let dir = temp_dir("ws-invite-checks");
        let all = Workspaces::open(&dir, 30, None).unwrap();
        let ws = all.create("Team", "acct_owner").unwrap();
        let (_, code) = all.invite(&ws.id, Role::Viewer, Some("Bob@Example.com".into()), 1, 1, "acct_owner").unwrap();
        assert!(matches!(all.redeem(&code, "acct_x", Some("eve@example.com"), 100), Err(ServerError::Forbidden(_))));
        assert!(matches!(all.redeem(&code, "acct_x", None, 100), Err(ServerError::Forbidden(_))));
        assert!(all.redeem(&format!("{code}x"), "acct_x", Some("bob@example.com"), 100).is_err());
        assert!(all.redeem("gci1.ws_nope.x", "acct_x", None, 100).is_err());
        assert!(all.redeem(&code, "acct_bob", Some("bob@example.com"), 100).is_ok());
        assert!(all.invite(&ws.id, Role::Owner, None, 5, 24, "acct_owner").is_err());
        assert!(all.invite(&ws.id, Role::Viewer, None, 1, 24 * 365, "acct_owner").is_err());
    }

    #[test]
    fn redeem_never_lowers_a_role() {
        let dir = temp_dir("ws-invite-role");
        let all = Workspaces::open(&dir, 30, None).unwrap();
        let ws = all.create("Team", "acct_owner").unwrap();
        let (_, code) = all.invite(&ws.id, Role::Viewer, None, 1, 24, "acct_owner").unwrap();
        let (_, role) = all.redeem(&code, "acct_owner", None, 100).unwrap();
        assert_eq!(role, Role::Owner);
    }

    #[test]
    fn keeps_an_owner() {
        let dir = temp_dir("ws-owner");
        let all = Workspaces::open(&dir, 30, None).unwrap();
        let ws = all.create("Team", "acct_owner").unwrap();
        assert!(matches!(
            all.set_role(&ws.id, "acct_owner", Role::Viewer, "acct_owner", 100),
            Err(ServerError::Conflict(_))
        ));
        assert!(matches!(all.remove_member(&ws.id, "acct_owner"), Err(ServerError::Conflict(_))));
        all.set_role(&ws.id, "acct_two", Role::Owner, "acct_owner", 100).unwrap();
        all.set_role(&ws.id, "acct_owner", Role::Viewer, "acct_two", 100).unwrap();
        all.remove_member(&ws.id, "acct_owner").unwrap();
        assert!(all.set_role(&ws.id, "acct_3", Role::Viewer, "acct_two", 1).is_err());
    }

    #[test]
    fn sees_invites_another_process_made() {
        let dir = temp_dir("ws-two-processes");
        let server = Workspaces::open(&dir, 30, None).unwrap();
        let ws = server.create("Team", "acct_owner").unwrap();
        // `graphrt collab invite` opens the same directory in another process.
        let cli = Workspaces::open(&dir, 30, None).unwrap();
        let (_, code) = cli.invite(&ws.id, Role::Operator, None, 1, 24, "system").unwrap();
        let new_ws = cli.create_unowned("Second").unwrap();
        let (_, second_code) = cli.invite(&new_ws, Role::Owner, None, 1, 24, "system").unwrap();
        assert_eq!(server.redeem(&code, "acct_a", None, 100).unwrap().1, Role::Operator);
        assert_eq!(server.redeem(&second_code, "acct_b", None, 100).unwrap().1, Role::Owner);
        assert!(server.get(&new_ws).is_some());
    }

    #[test]
    fn projects_and_limits() {
        let dir = temp_dir("ws-projects");
        let all = Workspaces::open(&dir, 30, None).unwrap();
        let ws = all.create("Team", "acct_owner").unwrap();
        let project = all.add_project(&ws.id, "Support bot", "acct_owner", 1).unwrap();
        assert!(matches!(all.add_project(&ws.id, "Two", "acct_owner", 1), Err(ServerError::Limit(_))));
        all.rename_project(&ws.id, &project, "Renamed").unwrap();
        assert_eq!(all.get(&ws.id).unwrap().projects[&project].name, "Renamed");
        all.remove_project(&ws.id, &project).unwrap();
        assert!(all.create("", "acct").is_err());
        assert!(all.create("a\u{7}b", "acct").is_err());
        assert!(all.files(&ws.id).is_ok());
        assert!(all.files("ws_../../etc").is_err());
        all.delete(&ws.id).unwrap();
        assert!(all.get(&ws.id).is_none());
    }
}
