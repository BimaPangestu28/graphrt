//! Accounts and their sign ins (refresh tokens).
//!
//! An account is made the first time someone signs in with the identity
//! provider, or redeems an invite code on a server that lets invite codes
//! make accounts. Each account is one file, `accounts/<id>.json`, holding the
//! account and the hashes of its refresh tokens.
//!
//! Refresh tokens (`gcr1.<account>.<secret>`) last `refresh_token_days`
//! without use and rotate on every use: each use gives a new one and retires
//! the old. A retired token used again means it was copied, so the whole
//! chain it belongs to ends (every device signed in by that sign in has to
//! sign in again).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use super::error::{ServerError, ServerResult};
use super::util::{
    MS_PER_DAY, constant_time_eq, ensure_dir, is_id, now_ms, random_id, random_token, read_json, sha256_hex, write_json,
};

const REFRESH_PREFIX: &str = "gcr1.";
/// Sign ins (refresh token chains) kept per account, the oldest go first.
const MAX_SIGN_INS: usize = 20;
/// Retired tokens are kept this long to notice a copied one being used.
const RETIRED_KEEP_MS: u64 = 7 * MS_PER_DAY;

/// How an account signs in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    /// With the identity provider.
    Oidc,
    /// Made by an invite code, signs in with its refresh token only.
    Invite,
}

/// One refresh token, hashed.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct RefreshRecord {
    /// The chain of rotations it belongs to (one sign in on one device).
    family: String,
    hash: String,
    created_ms: u64,
    expires_ms: u64,
    /// Used once already (a newer token replaced it).
    #[serde(default)]
    retired: bool,
    /// What signed in, e.g. `Graphrt Desktop on laptop`.
    #[serde(default)]
    device: String,
}

/// An account.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub kind: AccountKind,
    /// `<issuer> <sub>` for provider accounts, empty for invite accounts.
    #[serde(default)]
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub name: String,
    /// Groups of the last provider sign in (for role mapping).
    #[serde(default)]
    pub groups: Vec<String>,
    pub created_ms: u64,
    #[serde(default)]
    pub last_seen_ms: u64,
    /// Disabled accounts cannot sign in or refresh.
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    refresh: Vec<RefreshRecord>,
}

impl Account {
    /// The identity as the console's role mapping reads it.
    pub fn identity(&self) -> graphrt_console::roles::Identity {
        graphrt_console::roles::Identity {
            subject: self.subject.clone(),
            email: self.email.clone(),
            name: Some(self.name.clone()),
            groups: self.groups.clone(),
        }
    }

    /// What may be shown of the account to other members.
    pub fn public(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "name": self.name,
            "email": self.email,
            "color": crate::protocol::color_of(&self.id),
        })
    }
}

/// Who signs in, from the provider or an invite.
#[derive(Clone, Debug)]
pub struct NewIdentity {
    pub kind: AccountKind,
    pub subject: String,
    pub email: Option<String>,
    pub name: String,
    pub groups: Vec<String>,
}

/// Every account, in memory and on disk.
#[derive(Debug)]
pub struct Accounts {
    dir: PathBuf,
    refresh_days: u64,
    inner: RwLock<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    by_id: HashMap<String, Account>,
    by_subject: HashMap<String, String>,
}

impl Accounts {
    /// The accounts in `data_dir/accounts`.
    ///
    /// @throws ServerError::Io - When the directory cannot be read.
    pub fn open(data_dir: &std::path::Path, refresh_days: u64) -> ServerResult<Self> {
        let dir = data_dir.join("accounts");
        ensure_dir(&dir)?;
        let mut inner = Inner::default();
        let entries = std::fs::read_dir(&dir)
            .map_err(|error| ServerError::Io(format!("cannot read {}: {error}", dir.display())))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(account) = read_json::<Account>(&path)? {
                if !account.subject.is_empty() {
                    inner.by_subject.insert(account.subject.clone(), account.id.clone());
                }
                inner.by_id.insert(account.id.clone(), account);
            }
        }
        Ok(Self { dir, refresh_days, inner: RwLock::new(inner) })
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn save(&self, account: &Account) -> ServerResult<()> {
        write_json(&self.dir.join(format!("{}.json", account.id)), account)
    }

    /// The number of accounts.
    pub fn len(&self) -> usize {
        self.read().by_id.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The account `id`.
    pub fn get(&self, id: &str) -> Option<Account> {
        self.read().by_id.get(id).cloned()
    }

    /// The account of a provider identity, made or refreshed (name, email, groups).
    ///
    /// @throws ServerError - `Forbidden` for a disabled account, `Io` when it cannot be saved.
    pub fn upsert(&self, identity: NewIdentity) -> ServerResult<Account> {
        let mut inner = self.write();
        let existing = (!identity.subject.is_empty())
            .then(|| inner.by_subject.get(&identity.subject).cloned())
            .flatten()
            .and_then(|id| inner.by_id.get(&id).cloned());
        let account = match existing {
            Some(mut account) => {
                if account.disabled {
                    return Err(ServerError::Forbidden("this account is disabled".into()));
                }
                account.email = identity.email;
                account.name = identity.name;
                account.groups = identity.groups;
                account.last_seen_ms = now_ms();
                account
            }
            None => Account {
                id: random_id("acct_")?,
                kind: identity.kind,
                subject: identity.subject,
                email: identity.email,
                name: identity.name,
                groups: identity.groups,
                created_ms: now_ms(),
                last_seen_ms: now_ms(),
                disabled: false,
                refresh: Vec::new(),
            },
        };
        self.save(&account)?;
        if !account.subject.is_empty() {
            inner.by_subject.insert(account.subject.clone(), account.id.clone());
        }
        inner.by_id.insert(account.id.clone(), account.clone());
        Ok(account)
    }

    /// Start a sign in for `account_id`: a new refresh token chain.
    ///
    /// @returns The refresh token (shown once).
    /// @throws ServerError - `NotFound`, `Forbidden` (disabled), `Io`.
    pub fn sign_in(&self, account_id: &str, device: &str) -> ServerResult<String> {
        let family = random_id("fam_")?;
        self.issue(account_id, &family, device)
    }

    fn issue(&self, account_id: &str, family: &str, device: &str) -> ServerResult<String> {
        let secret = random_token()?;
        let token = format!("{REFRESH_PREFIX}{account_id}.{secret}");
        let mut inner = self.write();
        let account = inner.by_id.get_mut(account_id).ok_or_else(|| ServerError::NotFound("no such account".into()))?;
        if account.disabled {
            return Err(ServerError::Forbidden("this account is disabled".into()));
        }
        let now = now_ms();
        account.refresh.retain(|r| r.expires_ms > now && !(r.retired && r.created_ms + RETIRED_KEEP_MS < now));
        account.refresh.push(RefreshRecord {
            family: family.to_owned(),
            hash: sha256_hex(token.as_bytes()),
            created_ms: now,
            expires_ms: now + self.refresh_days * MS_PER_DAY,
            retired: false,
            device: device.chars().take(80).collect(),
        });
        // Keep the newest chains.
        let mut families: Vec<(u64, String)> = Vec::new();
        for record in account.refresh.iter().filter(|r| !r.retired) {
            families.push((record.created_ms, record.family.clone()));
        }
        families.sort();
        if families.len() > MAX_SIGN_INS {
            let drop: Vec<String> =
                families.iter().take(families.len() - MAX_SIGN_INS).map(|(_, f)| f.clone()).collect();
            account.refresh.retain(|r| !drop.contains(&r.family));
        }
        account.last_seen_ms = now;
        let snapshot = account.clone();
        drop(inner);
        self.save(&snapshot)?;
        Ok(token)
    }

    /// Trade a refresh token for a new one.
    ///
    /// @returns The account and the new refresh token.
    /// @throws ServerError::Unauthorized - For an unknown, expired or retired
    ///   token (a retired one also ends its chain).
    pub fn refresh(&self, token: &str) -> ServerResult<(Account, String)> {
        let invalid = || ServerError::Unauthorized("sign in again: the refresh token is not valid".into());
        let rest = token.strip_prefix(REFRESH_PREFIX).ok_or_else(invalid)?;
        let (account_id, _secret) = rest.split_once('.').ok_or_else(invalid)?;
        if !is_id(account_id, "acct_") {
            return Err(invalid());
        }
        let hash = sha256_hex(token.as_bytes());
        let now = now_ms();
        let (family, device) = {
            let mut inner = self.write();
            let account = inner.by_id.get_mut(account_id).ok_or_else(invalid)?;
            if account.disabled {
                return Err(ServerError::Forbidden("this account is disabled".into()));
            }
            let record = account
                .refresh
                .iter_mut()
                .find(|r| constant_time_eq(r.hash.as_bytes(), hash.as_bytes()))
                .ok_or_else(invalid)?;
            if record.retired {
                let family = record.family.clone();
                account.refresh.retain(|r| r.family != family);
                let snapshot = account.clone();
                drop(inner);
                self.save(&snapshot)?;
                return Err(ServerError::Unauthorized(
                    "this sign in was used from two places, so it ended: sign in again".into(),
                ));
            }
            if record.expires_ms <= now {
                return Err(invalid());
            }
            record.retired = true;
            (record.family.clone(), record.device.clone())
        };
        let token = self.issue(account_id, &family, &device)?;
        let account = self.get(account_id).ok_or_else(invalid)?;
        Ok((account, token))
    }

    /// End the sign in `token` belongs to.
    ///
    /// @returns Whether it was a valid token.
    pub fn sign_out(&self, token: &str) -> ServerResult<bool> {
        let Some((account_id, _)) = token.strip_prefix(REFRESH_PREFIX).and_then(|rest| rest.split_once('.')) else {
            return Ok(false);
        };
        let hash = sha256_hex(token.as_bytes());
        let mut inner = self.write();
        let Some(account) = inner.by_id.get_mut(account_id) else { return Ok(false) };
        let Some(family) = account.refresh.iter().find(|r| r.hash == hash).map(|r| r.family.clone()) else {
            return Ok(false);
        };
        account.refresh.retain(|r| r.family != family);
        let snapshot = account.clone();
        drop(inner);
        self.save(&snapshot)?;
        Ok(true)
    }

    /// Disable or enable an account (a disabled one loses every sign in).
    pub fn set_disabled(&self, account_id: &str, disabled: bool) -> ServerResult<()> {
        let mut inner = self.write();
        let account = inner.by_id.get_mut(account_id).ok_or_else(|| ServerError::NotFound("no such account".into()))?;
        account.disabled = disabled;
        if disabled {
            account.refresh.clear();
        }
        let snapshot = account.clone();
        drop(inner);
        self.save(&snapshot)
    }

    /// Sign ins of an account, for its owner to see: `(device, created_ms, expires_ms)`.
    pub fn sign_ins(&self, account_id: &str) -> Vec<(String, u64, u64)> {
        self.get(account_id)
            .map(|a| {
                a.refresh
                    .iter()
                    .filter(|r| !r.retired)
                    .map(|r| (r.device.clone(), r.created_ms, r.expires_ms))
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("graphrt-collab-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn identity(subject: &str) -> NewIdentity {
        NewIdentity {
            kind: AccountKind::Oidc,
            subject: subject.into(),
            email: Some("ann@example.com".into()),
            name: "Ann".into(),
            groups: vec![],
        }
    }

    #[test]
    fn upsert_keeps_one_account_per_subject() {
        let dir = temp_dir("accounts-upsert");
        let accounts = Accounts::open(&dir, 30).unwrap();
        let a = accounts.upsert(identity("https://id sub1")).unwrap();
        let mut renamed = identity("https://id sub1");
        renamed.name = "Ann B".into();
        let b = accounts.upsert(renamed).unwrap();
        assert_eq!(a.id, b.id);
        assert_eq!(accounts.get(&a.id).unwrap().name, "Ann B");
        let reopened = Accounts::open(&dir, 30).unwrap();
        assert_eq!(reopened.len(), 1);
        assert_eq!(reopened.upsert(identity("https://id sub1")).unwrap().id, a.id);
    }

    #[test]
    fn refresh_rotates_and_detects_reuse() {
        let dir = temp_dir("accounts-refresh");
        let accounts = Accounts::open(&dir, 30).unwrap();
        let account = accounts.upsert(identity("s")).unwrap();
        let first = accounts.sign_in(&account.id, "laptop").unwrap();
        let (_, second) = accounts.refresh(&first).unwrap();
        let (_, third) = accounts.refresh(&second).unwrap();
        // Using a retired token ends the chain, the newest token included.
        assert!(matches!(accounts.refresh(&first), Err(ServerError::Unauthorized(_))));
        assert!(accounts.refresh(&third).is_err());
        // Another sign in is not affected.
        let other = accounts.sign_in(&account.id, "phone").unwrap();
        assert!(accounts.refresh(&other).is_ok());
    }

    #[test]
    fn refuses_bad_tokens_and_disabled_accounts() {
        let dir = temp_dir("accounts-bad");
        let accounts = Accounts::open(&dir, 30).unwrap();
        let account = accounts.upsert(identity("s")).unwrap();
        let token = accounts.sign_in(&account.id, "").unwrap();
        assert!(accounts.refresh("gcr1.acct_x.y").is_err());
        assert!(accounts.refresh(&format!("{token}x")).is_err());
        assert!(accounts.refresh("nope").is_err());
        accounts.set_disabled(&account.id, true).unwrap();
        assert!(accounts.refresh(&token).is_err());
        assert!(accounts.upsert(identity("s")).is_err());
    }

    #[test]
    fn sign_out_ends_the_chain() {
        let dir = temp_dir("accounts-signout");
        let accounts = Accounts::open(&dir, 30).unwrap();
        let account = accounts.upsert(identity("s")).unwrap();
        let token = accounts.sign_in(&account.id, "").unwrap();
        assert!(accounts.sign_out(&token).unwrap());
        assert!(accounts.refresh(&token).is_err());
        assert!(!accounts.sign_out(&token).unwrap());
    }
}
