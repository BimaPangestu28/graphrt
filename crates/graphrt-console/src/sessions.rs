//! Browser sessions of the people who signed in, and the sign ins on their way.
//!
//! A session token is `grs1.<256 random bits>` in an `HttpOnly`, `SameSite=Strict`
//! cookie (`__Host-` and `Secure` over HTTPS). The server keeps its SHA-256
//! only, in `sessions.json`, so sessions survive a restart. Each session has
//! its own CSRF token, which every request that changes something must send
//! in `X-Graphrt-CSRF`.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::config::SessionsConfig;
use crate::error::Result;
use crate::roles::Identity;
use crate::util::{JsonFile, MS_PER_MINUTE, constant_time_eq, now_ms, random_id, random_token, sha256_hex};

/// Prefix of session tokens; the digit is the format version.
pub const TOKEN_PREFIX: &str = "grs1.";
/// How often "last seen" is written at most.
const TOUCH_EVERY_MS: u64 = MS_PER_MINUTE;
/// Sessions kept at most; the oldest go first.
const MAX_SESSIONS: usize = 10_000;
/// How long a sign in may take at the provider.
pub const LOGIN_TTL_MS: u64 = 10 * MS_PER_MINUTE;
/// Sign ins on their way at most (a flood of them pushes out the oldest).
const MAX_PENDING: usize = 1_000;

/// A signed in browser.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// A public id (lists, sign out of one session).
    pub id: String,
    pub identity: Identity,
    pub csrf: String,
    pub created_ms: u64,
    pub last_seen_ms: u64,
    pub expires_ms: u64,
    token_hash: String,
}

/// The sessions, in `sessions.json`.
#[derive(Debug)]
pub struct Sessions {
    file: JsonFile<BTreeMap<String, Session>>,
    idle_ms: u64,
    max_ms: u64,
}

impl Sessions {
    /// The sessions kept in `dir`.
    ///
    /// @throws ConsoleError::Io - When the file cannot be read.
    pub fn open(dir: &Path, config: &SessionsConfig) -> Result<Self> {
        Ok(Self {
            file: JsonFile::open(dir.join("sessions.json"))?,
            idle_ms: config.idle_minutes.saturating_mul(MS_PER_MINUTE),
            max_ms: config.max_hours.saturating_mul(60 * MS_PER_MINUTE),
        })
    }

    /// How long a new session lasts at most, in seconds (the cookie's `Max-Age`).
    pub fn max_age_secs(&self) -> u64 {
        self.max_ms / 1000
    }

    /// Start a session for `identity`.
    ///
    /// @returns The token for the cookie (never stored) and the session.
    /// @throws ConsoleError - When there is no randomness or the file cannot be written.
    pub fn create(&self, identity: Identity) -> Result<(String, Session)> {
        let token = format!("{TOKEN_PREFIX}{}", random_token()?);
        let now = now_ms();
        let session = Session {
            id: random_id()?,
            identity,
            csrf: random_token()?,
            created_ms: now,
            last_seen_ms: now,
            expires_ms: now.saturating_add(self.max_ms),
            token_hash: sha256_hex(token.as_bytes()),
        };
        let stored = session.clone();
        self.file.update(move |sessions| {
            sessions.retain(|_, s| s.is_live(now, self.idle_ms));
            while sessions.len() >= MAX_SESSIONS {
                let oldest = sessions.iter().min_by_key(|(_, s)| s.last_seen_ms).map(|(id, _)| id.clone());
                match oldest {
                    Some(id) => sessions.remove(&id),
                    None => break,
                };
            }
            sessions.insert(stored.id.clone(), stored);
            Ok(())
        })?;
        Ok((token, session))
    }

    /// The live session of `token`, which counts as a use of it.
    ///
    /// @throws ConsoleError::Io - When the file changed and cannot be read.
    pub fn lookup(&self, token: &str) -> Result<Option<Session>> {
        if !token.starts_with(TOKEN_PREFIX) {
            return Ok(None);
        }
        let hash = sha256_hex(token.as_bytes());
        let now = now_ms();
        let found = self.file.read(|sessions| {
            sessions.values().find(|s| constant_time_eq(s.token_hash.as_bytes(), hash.as_bytes())).cloned()
        })?;
        let Some(session) = found.filter(|s| s.is_live(now, self.idle_ms)) else { return Ok(None) };
        if now.saturating_sub(session.last_seen_ms) >= TOUCH_EVERY_MS {
            let id = session.id.clone();
            let touched = self.file.update(|sessions| {
                if let Some(stored) = sessions.get_mut(&id) {
                    stored.last_seen_ms = now;
                }
                Ok(())
            });
            if let Err(error) = touched {
                // A failed touch only shortens the session; the request goes on.
                eprintln!("warning: console: cannot record a session's use: {error}");
            }
        }
        Ok(Some(session))
    }

    /// End the session `id`.
    ///
    /// @returns Whether there was one.
    /// @throws ConsoleError::Io - When the file cannot be written.
    pub fn end(&self, id: &str) -> Result<bool> {
        self.file.update(|sessions| Ok(sessions.remove(id).is_some()))
    }

    /// End every session of `subject` (a role was taken away).
    ///
    /// @returns How many ended.
    /// @throws ConsoleError::Io - When the file cannot be written.
    pub fn end_all_of(&self, subject: &str) -> Result<usize> {
        self.file.update(|sessions| {
            let before = sessions.len();
            sessions.retain(|_, s| s.identity.subject != subject);
            Ok(before - sessions.len())
        })
    }

    /// The live sessions, newest first.
    ///
    /// @throws ConsoleError::Io - When the file changed and cannot be read.
    pub fn list(&self) -> Result<Vec<Session>> {
        let now = now_ms();
        let mut live = self
            .file
            .read(|sessions| sessions.values().filter(|s| s.is_live(now, self.idle_ms)).cloned().collect::<Vec<_>>())?;
        live.sort_by_key(|s| std::cmp::Reverse(s.created_ms));
        Ok(live)
    }
}

impl Session {
    fn is_live(&self, now: u64, idle_ms: u64) -> bool {
        now < self.expires_ms && now.saturating_sub(self.last_seen_ms) < idle_ms
    }

    /// Whether `token` is this session's CSRF token.
    pub fn csrf_matches(&self, token: &str) -> bool {
        !token.is_empty() && constant_time_eq(self.csrf.as_bytes(), token.as_bytes())
    }

    /// The session as listed (no token, no CSRF token).
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "subject": self.identity.subject,
            "email": self.identity.email,
            "name": self.identity.name,
            "created_ms": self.created_ms,
            "last_seen_ms": self.last_seen_ms,
            "expires_ms": self.expires_ms,
        })
    }
}

/// A sign in on its way: what the callback checks.
#[derive(Clone, Debug)]
pub struct PendingLogin {
    /// SHA-256 of the value of the browser's login cookie: the callback must
    /// come from the browser that started the sign in.
    pub binding_hash: String,
    pub verifier: String,
    pub nonce: String,
    /// Where the browser goes after the sign in (a path of this server).
    pub return_to: String,
    pub created_ms: u64,
}

/// Sign ins on their way, by `state`, in memory: a restart only means signing
/// in again.
#[derive(Debug, Default)]
pub struct PendingLogins {
    pending: Mutex<HashMap<String, PendingLogin>>,
}

impl PendingLogins {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, PendingLogin>> {
        self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Keep `login` under `state`.
    pub fn insert(&self, state: String, login: PendingLogin) {
        let now = now_ms();
        let mut pending = self.lock();
        pending.retain(|_, login| now.saturating_sub(login.created_ms) < LOGIN_TTL_MS);
        while pending.len() >= MAX_PENDING {
            let oldest = pending.iter().min_by_key(|(_, login)| login.created_ms).map(|(state, _)| state.clone());
            match oldest {
                Some(state) => pending.remove(&state),
                None => break,
            };
        }
        pending.insert(state, login);
    }

    /// Take the sign in of `state`: each can be completed once.
    pub fn take(&self, state: &str) -> Option<PendingLogin> {
        let login = self.lock().remove(state)?;
        (now_ms().saturating_sub(login.created_ms) < LOGIN_TTL_MS).then_some(login)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sessions(dir: &Path, idle_minutes: u64) -> Sessions {
        Sessions::open(dir, &SessionsConfig { idle_minutes, max_hours: 1 }).unwrap()
    }

    fn person(subject: &str) -> Identity {
        Identity { subject: subject.into(), ..Identity::default() }
    }

    #[test]
    fn a_token_finds_its_session_until_it_ends() {
        let dir = graphrt_core::private_fs::create_fresh_temp_dir("graphrt-console-sessions").unwrap();
        let store = sessions(&dir, 60);
        let (token, session) = store.create(person("a")).unwrap();
        assert!(token.starts_with(TOKEN_PREFIX));
        assert_eq!(store.lookup(&token).unwrap().map(|s| s.id), Some(session.id.clone()));
        assert!(store.lookup("grs1.wrong").unwrap().is_none());
        assert!(store.lookup(&token.replace("grs1.", "grd1.")).unwrap().is_none());
        // Kept across a reopen: the file holds the hash only.
        let text = std::fs::read_to_string(dir.join("sessions.json")).unwrap();
        assert!(!text.contains(token.trim_start_matches(TOKEN_PREFIX)));
        assert!(sessions(&dir, 60).lookup(&token).unwrap().is_some());
        assert!(store.end(&session.id).unwrap());
        assert!(store.lookup(&token).unwrap().is_none());
        let _cleanup = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_person_can_be_signed_out_everywhere() {
        let dir = graphrt_core::private_fs::create_fresh_temp_dir("graphrt-console-sessions-all").unwrap();
        let store = sessions(&dir, 60);
        let (one, _) = store.create(person("a")).unwrap();
        let (two, _) = store.create(person("a")).unwrap();
        let (other, _) = store.create(person("b")).unwrap();
        assert_eq!(store.end_all_of("a").unwrap(), 2);
        assert!(store.lookup(&one).unwrap().is_none() && store.lookup(&two).unwrap().is_none());
        assert!(store.lookup(&other).unwrap().is_some());
        let _cleanup = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn csrf_tokens_must_match_exactly() {
        let dir = graphrt_core::private_fs::create_fresh_temp_dir("graphrt-console-csrf").unwrap();
        let (_, session) = sessions(&dir, 60).create(person("a")).unwrap();
        assert!(session.csrf_matches(&session.csrf));
        assert!(!session.csrf_matches(""));
        assert!(!session.csrf_matches(&session.csrf[1..]));
        let _cleanup = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn idle_and_old_sessions_are_not_live() {
        let session = Session {
            id: "x".into(),
            identity: person("a"),
            csrf: "c".into(),
            created_ms: 0,
            last_seen_ms: 1_000,
            expires_ms: 100_000,
            token_hash: String::new(),
        };
        assert!(session.is_live(50_000, 60_000));
        assert!(!session.is_live(70_000, 60_000), "idle too long");
        assert!(!session.is_live(100_000, 1_000_000), "past its end");
    }

    #[test]
    fn a_pending_login_is_taken_once() {
        let pending = PendingLogins::default();
        let login = PendingLogin {
            binding_hash: "h".into(),
            verifier: "v".into(),
            nonce: "n".into(),
            return_to: "/console/".into(),
            created_ms: now_ms(),
        };
        pending.insert("s".into(), login);
        assert!(pending.take("s").is_some());
        assert!(pending.take("s").is_none());
        let old = PendingLogin { created_ms: 0, ..pending_login() };
        pending.insert("old".into(), old);
        assert!(pending.take("old").is_none());
    }

    fn pending_login() -> PendingLogin {
        PendingLogin {
            binding_hash: String::new(),
            verifier: String::new(),
            nonce: String::new(),
            return_to: String::new(),
            created_ms: 0,
        }
    }
}
