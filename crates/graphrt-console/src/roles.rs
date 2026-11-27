//! Roles, who gets which, and the people who signed in.
//!
//! | Role | May |
//! | --- | --- |
//! | `viewer` | read: workflows, runs, the approvals inbox, the registry |
//! | `operator` | what a viewer may, and start, stop and resume runs, answer approvals, pair and remove devices, see which secrets exist |
//! | `owner` | everything: versions and promotions, secrets, roles, the audit log, skills, lessons, knowledge |
//!
//! A person's role is mapped from the ID token at each sign in ([`RolesConfig`]),
//! unless an owner set one in the console, which then applies at once. The
//! owners of the config are owners whatever the console says.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::RolesConfig;
use crate::error::{ConsoleError, Result};
use crate::util::{JsonFile, now_ms};

/// What a person may do. Each role may do what the ones below it may.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Viewer,
    Operator,
    Owner,
}

impl Role {
    /// The role as the API writes it.
    pub fn name(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Operator => "operator",
            Role::Owner => "owner",
        }
    }
}

/// A role an owner set in the console: a role, or no access at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Assigned {
    Viewer,
    Operator,
    Owner,
    /// Signed out at once and kept out.
    None,
}

impl Assigned {
    fn role(self) -> Option<Role> {
        match self {
            Assigned::Viewer => Some(Role::Viewer),
            Assigned::Operator => Some(Role::Operator),
            Assigned::Owner => Some(Role::Owner),
            Assigned::None => None,
        }
    }
}

/// Who signed in, from the verified ID token.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// `sub`: the person at the provider.
    pub subject: String,
    /// `email`, only when the provider verified it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Values of the groups claim.
    #[serde(default)]
    pub groups: Vec<String>,
}

impl Identity {
    /// The identity in the claims of a verified ID token. An `email` the
    /// provider marks unverified (`email_verified: false`) is left out.
    pub fn from_claims(claims: &serde_json::Map<String, serde_json::Value>, groups_claim: &str) -> Option<Self> {
        let text = |name: &str| claims.get(name).and_then(serde_json::Value::as_str).map(str::trim);
        let subject = text("sub").filter(|sub| !sub.is_empty())?.to_owned();
        let verified = !matches!(claims.get("email_verified"), Some(serde_json::Value::Bool(false)));
        let email = text("email").filter(|email| verified && email.contains('@')).map(str::to_ascii_lowercase);
        let name = text("name").filter(|name| !name.is_empty()).map(str::to_owned);
        let groups = match claims.get(groups_claim) {
            Some(serde_json::Value::Array(values)) => {
                values.iter().filter_map(serde_json::Value::as_str).map(str::to_owned).collect()
            }
            Some(serde_json::Value::String(one)) => vec![one.clone()],
            _ => Vec::new(),
        };
        Some(Self { subject, email, name, groups })
    }
}

/// Whether `identity` is an owner of the config (by email or subject).
pub fn is_config_owner(config: &RolesConfig, identity: &Identity) -> bool {
    config.owners.iter().any(|owner| {
        owner == &identity.subject || identity.email.as_deref().is_some_and(|email| owner.eq_ignore_ascii_case(email))
    })
}

/// The role the config gives `identity`, `None` for no access.
pub fn mapped_role(config: &RolesConfig, identity: &Identity) -> Option<Role> {
    if is_config_owner(config, identity) {
        return Some(Role::Owner);
    }
    if !config.allowed_domains.is_empty() {
        let domain = identity.email.as_deref().and_then(|email| email.rsplit_once('@')).map(|(_, domain)| domain);
        let allowed =
            domain.is_some_and(|domain| config.allowed_domains.iter().any(|d| d.eq_ignore_ascii_case(domain)));
        if !allowed {
            return None;
        }
    }
    let from_groups = identity.groups.iter().filter_map(|group| config.groups.get(group).copied()).max();
    from_groups.or(config.default_role)
}

/// A person who signed in at least once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The role the config gave at the last sign in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapped: Option<Role>,
    /// The role an owner set, which replaces `mapped`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned: Option<Assigned>,
    /// Whether the config names this person an owner.
    #[serde(default)]
    pub config_owner: bool,
    pub first_seen_ms: u64,
    pub last_sign_in_ms: u64,
}

impl Member {
    /// What the person may do now.
    pub fn role(&self) -> Option<Role> {
        if self.config_owner {
            return Some(Role::Owner);
        }
        match self.assigned {
            Some(assigned) => assigned.role(),
            None => self.mapped,
        }
    }

    /// The member as listed.
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "subject": self.subject,
            "email": self.email,
            "name": self.name,
            "role": self.role(),
            "mapped_role": self.mapped,
            "assigned_role": self.assigned,
            "config_owner": self.config_owner,
            "first_seen_ms": self.first_seen_ms,
            "last_sign_in_ms": self.last_sign_in_ms,
        })
    }
}

/// The most people the console keeps.
pub const MAX_MEMBERS: usize = 10_000;

/// Everyone who signed in, in `members.json`.
#[derive(Debug)]
pub struct Members {
    file: JsonFile<BTreeMap<String, Member>>,
}

impl Members {
    /// The members kept in `dir`.
    ///
    /// @throws ConsoleError::Io - When the file cannot be read.
    pub fn open(dir: &Path) -> Result<Self> {
        Ok(Self { file: JsonFile::open(dir.join("members.json"))? })
    }

    /// Record a sign in and answer the person's role now (`None`: no access).
    ///
    /// @throws ConsoleError::Io - When the file cannot be written.
    pub fn signed_in(&self, config: &RolesConfig, identity: &Identity) -> Result<Option<Role>> {
        let now = now_ms();
        let mapped = mapped_role(config, identity);
        let config_owner = is_config_owner(config, identity);
        self.file.update(|members| {
            if !members.contains_key(&identity.subject) && members.len() >= MAX_MEMBERS {
                return Err(ConsoleError::Conflict(format!("the console keeps at most {MAX_MEMBERS} people")));
            }
            let member = members.entry(identity.subject.clone()).or_insert_with(|| Member {
                subject: identity.subject.clone(),
                email: None,
                name: None,
                mapped: None,
                assigned: None,
                config_owner: false,
                first_seen_ms: now,
                last_sign_in_ms: now,
            });
            member.email.clone_from(&identity.email);
            member.name.clone_from(&identity.name);
            member.mapped = mapped;
            member.config_owner = config_owner;
            member.last_sign_in_ms = now;
            Ok(member.role())
        })
    }

    /// The role of `subject` now (`None`: unknown or no access).
    ///
    /// @throws ConsoleError::Io - When the file changed and cannot be read.
    pub fn role_of(&self, subject: &str) -> Result<Option<Role>> {
        self.file.read(|members| members.get(subject).and_then(Member::role))
    }

    /// The member `subject`.
    ///
    /// @throws ConsoleError::Io - When the file changed and cannot be read.
    pub fn get(&self, subject: &str) -> Result<Option<Member>> {
        self.file.read(|members| members.get(subject).cloned())
    }

    /// Everyone, most recently signed in first.
    ///
    /// @throws ConsoleError::Io - When the file changed and cannot be read.
    pub fn list(&self) -> Result<Vec<Member>> {
        let mut members = self.file.read(|members| members.values().cloned().collect::<Vec<_>>())?;
        members.sort_by_key(|m| std::cmp::Reverse(m.last_sign_in_ms));
        Ok(members)
    }

    /// Set (or with `None` clear) the role an owner gives `subject`. The last
    /// owner cannot be demoted, and the owners of the config not at all.
    ///
    /// @returns The member as changed.
    /// @throws ConsoleError - `NotFound` for someone who never signed in,
    ///   `Conflict` for a config owner or the last owner, `Io` on a failed write.
    pub fn assign(&self, subject: &str, assigned: Option<Assigned>) -> Result<Member> {
        self.file.update(|members| {
            let current = members
                .get(subject)
                .ok_or_else(|| ConsoleError::NotFound(format!("nobody with subject `{subject}` signed in yet")))?;
            if current.config_owner {
                return Err(ConsoleError::Conflict(
                    "this person is an owner in the console config: change it there".into(),
                ));
            }
            let mut changed = current.clone();
            changed.assigned = assigned;
            let was_owner = current.role() == Some(Role::Owner);
            if was_owner && changed.role() != Some(Role::Owner) {
                let others = members.values().filter(|m| m.subject != subject && m.role() == Some(Role::Owner)).count();
                if others == 0 {
                    return Err(ConsoleError::Conflict("the last owner cannot be demoted".into()));
                }
            }
            members.insert(subject.to_owned(), changed.clone());
            Ok(changed)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roles() -> RolesConfig {
        RolesConfig {
            owners: vec!["boss@example.com".into()],
            groups: [("ops".to_owned(), Role::Operator), ("staff".to_owned(), Role::Viewer)].into_iter().collect(),
            default_role: None,
            allowed_domains: Vec::new(),
        }
    }

    fn identity(email: &str, groups: &[&str]) -> Identity {
        Identity {
            subject: format!("sub-{email}"),
            email: Some(email.into()),
            name: None,
            groups: groups.iter().map(|g| (*g).to_owned()).collect(),
        }
    }

    #[test]
    fn the_highest_mapped_role_wins() {
        let config = roles();
        assert_eq!(mapped_role(&config, &identity("a@example.com", &["staff", "ops"])), Some(Role::Operator));
        assert_eq!(mapped_role(&config, &identity("a@example.com", &["other"])), None);
        assert_eq!(mapped_role(&config, &identity("boss@example.com", &[])), Some(Role::Owner));
        let open = RolesConfig { default_role: Some(Role::Viewer), ..roles() };
        assert_eq!(mapped_role(&open, &identity("a@example.com", &[])), Some(Role::Viewer));
    }

    #[test]
    fn allowed_domains_keep_out_other_emails() {
        let config = RolesConfig { allowed_domains: vec!["example.com".into()], ..roles() };
        assert_eq!(mapped_role(&config, &identity("a@example.com", &["ops"])), Some(Role::Operator));
        assert_eq!(mapped_role(&config, &identity("a@evil.example", &["ops"])), None);
        let no_email = Identity { email: None, ..identity("a@example.com", &["ops"]) };
        assert_eq!(mapped_role(&config, &no_email), None);
    }

    #[test]
    fn unverified_emails_are_ignored() {
        let claims = json!({"sub": "1", "email": "boss@example.com", "email_verified": false, "groups": "ops"});
        let identity = Identity::from_claims(claims.as_object().unwrap(), "groups").unwrap();
        assert_eq!(identity.email, None);
        assert_eq!(identity.groups, ["ops"]);
        assert!(!is_config_owner(&roles(), &identity));
        let claims = json!({"sub": "", "email": "a@b.c"});
        assert!(Identity::from_claims(claims.as_object().unwrap(), "groups").is_none());
    }

    #[test]
    fn owners_assign_roles_but_never_remove_the_last_owner() {
        let dir = graphrt_core::private_fs::create_fresh_temp_dir("graphrt-console-members").unwrap();
        let members = Members::open(&dir).unwrap();
        let config = RolesConfig { owners: Vec::new(), ..roles() };
        let alice = identity("alice@example.com", &["ops"]);
        assert_eq!(members.signed_in(&config, &alice).unwrap(), Some(Role::Operator));
        members.assign(&alice.subject, Some(Assigned::Owner)).unwrap();
        assert_eq!(members.role_of(&alice.subject).unwrap(), Some(Role::Owner));
        let refused = members.assign(&alice.subject, Some(Assigned::Viewer));
        assert!(matches!(refused, Err(ConsoleError::Conflict(_))));
        let bob = identity("bob@example.com", &[]);
        assert_eq!(members.signed_in(&config, &bob).unwrap(), None);
        members.assign(&bob.subject, Some(Assigned::Owner)).unwrap();
        members.assign(&alice.subject, Some(Assigned::None)).unwrap();
        assert_eq!(members.role_of(&alice.subject).unwrap(), None);
        // A new sign in keeps what an owner set.
        assert_eq!(members.signed_in(&config, &alice).unwrap(), None);
        assert!(matches!(members.assign("nobody", None), Err(ConsoleError::NotFound(_))));
        let _cleanup = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn config_owners_cannot_be_demoted() {
        let dir = graphrt_core::private_fs::create_fresh_temp_dir("graphrt-console-owners").unwrap();
        let members = Members::open(&dir).unwrap();
        let boss = identity("boss@example.com", &[]);
        assert_eq!(members.signed_in(&roles(), &boss).unwrap(), Some(Role::Owner));
        assert!(matches!(members.assign(&boss.subject, Some(Assigned::None)), Err(ConsoleError::Conflict(_))));
        let _cleanup = std::fs::remove_dir_all(dir);
    }
}
