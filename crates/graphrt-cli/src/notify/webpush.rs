//! Web Push: notifications to browsers and installed web apps, without a
//! vendor account (`graphrt serve --push CONTACT`, cargo feature `web-push`).
//!
//! A browser subscribes at its vendor's push service and hands the server an
//! endpoint URL and two keys. To notify it, the server POSTs to that endpoint:
//!
//! - The message is encrypted for the browser (RFC 8291, `aes128gcm`): the
//!   push service relays it and cannot read it.
//! - The request is signed with the server's own key (VAPID, RFC 8292), which
//!   the browser named when it subscribed. No API key of Google, Apple or
//!   Mozilla is involved, and the server registers nowhere.
//!
//! The server's VAPID key is created on first use and kept in
//! `<store>/push/vapid.pkcs8`; subscriptions in `<store>/push/subscriptions.json`
//! (both owner-only). Without a store they live in memory and browsers must
//! subscribe again after a restart.
//!
//! Requests to push services go through the server's egress policy: `https`
//! only, no private addresses, no redirects. A subscription whose push service
//! answers `404` or `410` is gone and is removed.

use std::path::Path;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::future::BoxFuture;
use graphrt_core::private_fs;
use graphrt_core::store::now_ms;
use graphrt_http::{EgressPolicy, HttpClient};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use ring::{aead, agreement, hkdf};
use serde::{Deserialize, Serialize};

use super::{Notice, NoticeKind, Notifier, NotifyError};
use crate::error::CliError;
use crate::shared_file::SharedJsonFile;

/// The directory of the push files inside the store directory.
const DIR_NAME: &str = "push";
const KEY_FILE: &str = "vapid.pkcs8";
const SUBSCRIPTIONS_FILE: &str = "subscriptions.json";
/// The most subscriptions a server keeps, and the most of one device.
pub const MAX_SUBSCRIPTIONS: usize = 500;
pub const MAX_PER_DEVICE: usize = 5;
/// The longest endpoint URL, in bytes.
const MAX_ENDPOINT_BYTES: usize = 2000;
/// An uncompressed P-256 point.
const PUBLIC_KEY_BYTES: usize = 65;
const AUTH_SECRET_BYTES: usize = 16;
/// The record size announced in the message header (RFC 8188). The message is one record.
const RECORD_SIZE: u32 = 4096;
/// How long a VAPID token is valid. Push services accept up to 24 hours.
const VAPID_LIFETIME_SECS: u64 = 12 * 60 * 60;
/// How long a push service keeps a message for a device that is offline.
const TTL_SECS: &str = "86400";
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a push request failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum PushError {
    #[error("{0}")]
    Invalid(String),
    #[error("this server already has {MAX_SUBSCRIPTIONS} push subscriptions")]
    TooMany,
    #[error("push store: {0}")]
    Io(String),
    #[error("push encryption failed")]
    Crypto,
}

impl From<String> for PushError {
    fn from(message: String) -> Self {
        PushError::Io(message)
    }
}

/// What a browser hands over when it subscribes (`PushSubscription.toJSON()`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    /// The URL at the browser vendor's push service.
    pub endpoint: String,
    /// The browser's P-256 public key, base64url.
    pub p256dh: String,
    /// The browser's authentication secret, base64url.
    pub auth: String,
    /// The paired device that subscribed; `None` for the API key holder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(default)]
    pub created_ms: u64,
}

/// A subscription's keys, decoded and checked.
struct SubscriptionKeys {
    public: [u8; PUBLIC_KEY_BYTES],
    auth: [u8; AUTH_SECRET_BYTES],
}

impl Subscription {
    fn keys(&self) -> Result<SubscriptionKeys, PushError> {
        let decode = |text: &str| URL_SAFE_NO_PAD.decode(text.trim_end_matches('='));
        let public: [u8; PUBLIC_KEY_BYTES] = decode(&self.p256dh)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .filter(|bytes: &[u8; PUBLIC_KEY_BYTES]| bytes[0] == 4)
            .ok_or_else(|| PushError::Invalid("`keys.p256dh` is not a P-256 public key".to_owned()))?;
        let auth: [u8; AUTH_SECRET_BYTES] = decode(&self.auth)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| PushError::Invalid("`keys.auth` is not a 16 byte secret".to_owned()))?;
        Ok(SubscriptionKeys { public, auth })
    }
}

/// An HKDF output of a given length.
struct Len(usize);

impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn expand<const N: usize>(prk: &hkdf::Prk, info: &[&[u8]]) -> Result<[u8; N], PushError> {
    let mut out = [0_u8; N];
    prk.expand(info, Len(N)).and_then(|okm| okm.fill(&mut out)).map_err(|_| PushError::Crypto)?;
    Ok(out)
}

/// The content encryption key and nonce of a message (RFC 8291, section 3.4).
///
/// @param ecdh_secret - The shared secret of the sender's and the browser's keys.
/// @param ua_public - The browser's public key. `as_public` is the sender's, made for this message.
fn content_keys(
    ecdh_secret: &[u8],
    auth_secret: &[u8],
    ua_public: &[u8],
    as_public: &[u8],
    salt: &[u8],
) -> Result<([u8; 16], [u8; 12]), PushError> {
    let key_prk = hkdf::Salt::new(hkdf::HKDF_SHA256, auth_secret).extract(ecdh_secret);
    let ikm: [u8; 32] = expand(&key_prk, &[b"WebPush: info\0", ua_public, as_public])?;
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(&ikm);
    let key = expand(&prk, &[b"Content-Encoding: aes128gcm\0"])?;
    let nonce = expand(&prk, &[b"Content-Encoding: nonce\0"])?;
    Ok((key, nonce))
}

/// `plaintext` as the one record of a message: padded with the last-record
/// delimiter, sealed with AES-128-GCM (RFC 8188).
fn seal_record(key: &[u8; 16], nonce: [u8; 12], plaintext: &[u8]) -> Result<Vec<u8>, PushError> {
    let key =
        aead::UnboundKey::new(&aead::AES_128_GCM, key).map(aead::LessSafeKey::new).map_err(|_| PushError::Crypto)?;
    let mut record = Vec::with_capacity(plaintext.len() + 1 + aead::AES_128_GCM.tag_len());
    record.extend_from_slice(plaintext);
    record.push(2);
    key.seal_in_place_append_tag(aead::Nonce::assume_unique_for_key(nonce), aead::Aad::empty(), &mut record)
        .map_err(|_| PushError::Crypto)?;
    Ok(record)
}

/// The body of a push request: the `aes128gcm` header (salt, record size and
/// the sender's public key), then the record.
fn message_body(salt: &[u8; 16], as_public: &[u8], record: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(21 + as_public.len() + record.len());
    body.extend_from_slice(salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    // An uncompressed P-256 point is 65 bytes: it fits the one length byte.
    body.push(u8::try_from(as_public.len()).unwrap_or(u8::MAX));
    body.extend_from_slice(as_public);
    body.extend_from_slice(record);
    body
}

/// Encrypt `payload` for the browser of `keys`, with a fresh key pair and salt.
fn encrypt(payload: &[u8], keys: &SubscriptionKeys, random: &SystemRandom) -> Result<Vec<u8>, PushError> {
    // One record of `RECORD_SIZE`: the payload, its delimiter and the tag must fit.
    if payload.len() + 1 + aead::AES_128_GCM.tag_len() > RECORD_SIZE as usize {
        return Err(PushError::Invalid("the notification is too long".to_owned()));
    }
    let mut salt = [0_u8; 16];
    random.fill(&mut salt).map_err(|_| PushError::Crypto)?;
    let private =
        agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, random).map_err(|_| PushError::Crypto)?;
    let as_public = private.compute_public_key().map_err(|_| PushError::Crypto)?;
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, keys.public);
    let (key, nonce) = agreement::agree_ephemeral(private, &peer, |ecdh_secret| {
        content_keys(ecdh_secret, &keys.auth, &keys.public, as_public.as_ref(), &salt)
    })
    .map_err(|_| PushError::Invalid("`keys.p256dh` is not a point on the P-256 curve".to_owned()))??;
    let record = seal_record(&key, nonce, payload)?;
    Ok(message_body(&salt, as_public.as_ref(), &record))
}

/// The server's VAPID key: it signs every push request.
struct VapidKey {
    pair: EcdsaKeyPair,
    /// The public key as browsers take it (`applicationServerKey`), base64url.
    public: String,
}

impl std::fmt::Debug for VapidKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VapidKey").field("public", &self.public).finish_non_exhaustive()
    }
}

impl VapidKey {
    fn from_pkcs8(pkcs8: &[u8], random: &SystemRandom) -> Result<Self, String> {
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8, random)
            .map_err(|e| format!("not a P-256 private key: {e}"))?;
        let public = URL_SAFE_NO_PAD.encode(pair.public_key().as_ref());
        Ok(Self { pair, public })
    }

    #[cfg(feature = "store-s3")]
    /// The key of a shared store (created once: the server that loses the
    /// race to create it uses the winner's key).
    fn open_object(objects: &dyn crate::store::objects::Objects, random: &SystemRandom) -> Result<Self, String> {
        use crate::store::objects::{Condition, ObjectError, block_on, get};
        let key = format!("{DIR_NAME}/{KEY_FILE}");
        if let Some(object) = block_on(get(objects, &key)).map_err(|e| format!("{key}: {e}"))? {
            return Self::from_pkcs8(&object.bytes, random).map_err(|e| format!("{key}: {e}"));
        }
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, random)
            .map_err(|_| "no system randomness for a VAPID key".to_owned())?;
        match block_on(objects.write(&key, pkcs8.as_ref().to_vec(), Condition::Absent)) {
            Ok(_) => Self::from_pkcs8(pkcs8.as_ref(), random),
            Err(ObjectError::Precondition) => {
                let object = block_on(get(objects, &key))
                    .map_err(|e| format!("{key}: {e}"))?
                    .ok_or_else(|| format!("{key} vanished while it was created"))?;
                Self::from_pkcs8(&object.bytes, random).map_err(|e| format!("{key}: {e}"))
            }
            Err(error) => Err(format!("{key}: {error}")),
        }
    }

    /// The key in `dir` (created on first use), or a new one kept in memory.
    fn open(dir: Option<&Path>, random: &SystemRandom) -> Result<Self, String> {
        let generate = || {
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, random)
                .map_err(|_| "no system randomness for a VAPID key".to_owned())
        };
        let Some(dir) = dir else { return Self::from_pkcs8(generate()?.as_ref(), random) };
        let path = dir.join(KEY_FILE);
        match std::fs::read(&path) {
            Ok(pkcs8) => Self::from_pkcs8(&pkcs8, random).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let pkcs8 = generate()?;
                // Created exclusively: two servers starting together on one
                // directory must end up with the same key.
                match new_private_file(&path) {
                    Ok(mut file) => {
                        use std::io::Write as _;
                        file.write_all(pkcs8.as_ref())
                            .and_then(|()| file.sync_all())
                            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
                        Self::from_pkcs8(pkcs8.as_ref(), random)
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        let pkcs8 = std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                        Self::from_pkcs8(&pkcs8, random).map_err(|e| format!("{}: {e}", path.display()))
                    }
                    Err(e) => Err(format!("cannot create {}: {e}", path.display())),
                }
            }
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    /// The `Authorization` header of a request to the push service at
    /// `audience` (its origin), valid from `now_secs` (RFC 8292).
    fn authorization(
        &self,
        audience: &str,
        contact: &str,
        now_secs: u64,
        random: &SystemRandom,
    ) -> Result<String, PushError> {
        let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = serde_json::json!({ "aud": audience, "exp": now_secs + VAPID_LIFETIME_SECS, "sub": contact });
        let signed = format!("{header}.{}", URL_SAFE_NO_PAD.encode(claims.to_string()));
        let signature = self.pair.sign(random, signed.as_bytes()).map_err(|_| PushError::Crypto)?;
        Ok(format!("vapid t={signed}.{}, k={}", URL_SAFE_NO_PAD.encode(signature.as_ref()), self.public))
    }
}

/// Create `path` owner-only, failing when it exists already.
fn new_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(private_fs::PRIVATE_FILE_MODE);
    }
    options.open(path)
}

/// What became of one push request.
#[derive(Debug, PartialEq, Eq)]
enum Delivery {
    Sent,
    /// The push service no longer knows the subscription.
    Gone,
    Failed(String),
}

/// Web Push for a server.
#[derive(Debug)]
pub struct WebPush {
    key: VapidKey,
    /// `mailto:` or `https:` address a push service may contact about this sender.
    contact: String,
    subscriptions: SharedJsonFile<Vec<Subscription>>,
    client: HttpClient,
    /// Whether endpoints may be plain `http` and private addresses (`--allow-private-network`).
    allow_private: bool,
    random: SystemRandom,
}

impl WebPush {
    /// Web Push with its files in `<store_dir>/push`, or in memory without a store.
    ///
    /// @param contact - A `mailto:` or `https:` address for push services (the VAPID `sub`).
    /// @param allow_private - Whether push endpoints may be private or plain `http` addresses.
    /// @throws CliError (usage) - A contact of another form, or files that cannot be read or created.
    pub fn open(
        store: Option<&crate::store::StoreLocation>,
        contact: &str,
        allow_private: bool,
    ) -> Result<Self, CliError> {
        let contact = contact.trim();
        let is_address = contact.strip_prefix("mailto:").is_some_and(|address| address.contains('@'))
            || contact.strip_prefix("https://").is_some_and(|rest| !rest.is_empty());
        if !is_address || contact.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(CliError::usage(format!(
                "--push `{contact}`: expected an address push services may contact, such as mailto:you@example.com"
            )));
        }
        let random = SystemRandom::new();
        let (key, subscriptions) = match store {
            #[cfg(feature = "store-s3")]
            Some(crate::store::StoreLocation::Objects(objects)) => (
                VapidKey::open_object(objects.as_ref(), &random),
                SharedJsonFile::open_object(
                    std::sync::Arc::clone(objects),
                    &format!("{DIR_NAME}/{SUBSCRIPTIONS_FILE}"),
                ),
            ),
            Some(crate::store::StoreLocation::Dir(store)) => {
                let dir = store.join(DIR_NAME);
                private_fs::create_dir_all(&dir)
                    .map_err(|e| CliError::usage(format!("--push: cannot create `{}`: {e}", dir.display())))?;
                (VapidKey::open(Some(&dir), &random), SharedJsonFile::open(Some(dir.join(SUBSCRIPTIONS_FILE))))
            }
            None => (VapidKey::open(None, &random), SharedJsonFile::open(None)),
        };
        let key = key.map_err(|e| CliError::usage(format!("--push: {e}")))?;
        let subscriptions = subscriptions.map_err(|e| CliError::usage(format!("--push: {e}")))?;
        let policy = EgressPolicy { allow_private, max_redirects: 0, ..EgressPolicy::default() };
        let client = HttpClient::new(policy).map_err(|e| CliError::usage(format!("--push: {e}")))?;
        Ok(Self { key, contact: contact.to_owned(), subscriptions, client, allow_private, random })
    }

    /// One line for the startup log.
    pub fn describe(&self) -> String {
        let count = self.subscriptions.read(Vec::len).unwrap_or_default();
        let kept =
            if self.subscriptions.location().is_some() { "" } else { ", kept in memory (set --store to keep them)" };
        format!("web push: {count} subscription(s){kept}, when a run fails or waits for a person")
    }

    /// The key browsers subscribe with (`applicationServerKey`), base64url.
    pub fn public_key(&self) -> &str {
        &self.key.public
    }

    /// Check the endpoint of a subscription: a URL the egress policy lets the
    /// server call, `https` unless private addresses are allowed.
    fn endpoint_url(&self, endpoint: &str) -> Result<reqwest::Url, PushError> {
        let invalid = |why: String| PushError::Invalid(format!("`endpoint`: {why}"));
        if endpoint.len() > MAX_ENDPOINT_BYTES {
            return Err(invalid(format!("longer than {MAX_ENDPOINT_BYTES} bytes")));
        }
        let url = reqwest::Url::parse(endpoint).map_err(|e| invalid(e.to_string()))?;
        if url.scheme() != "https" && !(self.allow_private && url.scheme() == "http") {
            return Err(invalid("a push endpoint is an https URL".to_owned()));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(invalid("it must not carry credentials".to_owned()));
        }
        self.client.check_url(&url).map_err(|e| invalid(e.to_string()))?;
        Ok(url)
    }

    /// Keep `subscription`. One with the same endpoint is replaced; beyond
    /// [`MAX_PER_DEVICE`] the device's oldest goes.
    ///
    /// @throws PushError - `Invalid` for an endpoint or keys that cannot be used, `TooMany` at the limit.
    pub fn subscribe(&self, mut subscription: Subscription) -> Result<(), PushError> {
        self.endpoint_url(&subscription.endpoint)?;
        subscription.keys()?;
        subscription.created_ms = now_ms();
        self.subscriptions.update(|all| {
            all.retain(|known| known.endpoint != subscription.endpoint);
            let of_device = |known: &Subscription| known.device_id == subscription.device_id;
            while subscription.device_id.is_some()
                && all.iter().filter(|known| of_device(known)).count() >= MAX_PER_DEVICE
            {
                if let Some(oldest) = all.iter().position(of_device) {
                    all.remove(oldest);
                }
            }
            if all.len() >= MAX_SUBSCRIPTIONS {
                return Err(PushError::TooMany);
            }
            all.push(subscription.clone());
            Ok(())
        })
    }

    /// Forget the subscription of `endpoint`. A device removes only its own.
    /// Returns whether there was one.
    ///
    /// @throws PushError - `Io` when the file cannot be written.
    pub fn unsubscribe(&self, endpoint: &str, device_id: Option<&str>) -> Result<bool, PushError> {
        let own = |known: &Subscription| {
            known.endpoint == endpoint && (device_id.is_none() || known.device_id.as_deref() == device_id)
        };
        if !self.subscriptions.read(|all| all.iter().any(own))? {
            return Ok(false);
        }
        self.subscriptions.update(|all| {
            all.retain(|known| !own(known));
            Ok::<_, PushError>(true)
        })
    }

    /// Forget every subscription of device `id` (it was revoked).
    pub fn forget_device(&self, id: &str) {
        let of_device = |known: &Subscription| known.device_id.as_deref() == Some(id);
        if !self.subscriptions.read(|all| all.iter().any(of_device)).unwrap_or(false) {
            return;
        }
        let removed: Result<(), PushError> = self.subscriptions.update(|all| {
            all.retain(|known| !of_device(known));
            Ok(())
        });
        if let Err(error) = removed {
            eprintln!("warning: cannot remove the push subscriptions of device {id}: {error}");
        }
    }

    /// Send a test notification to the subscription of `endpoint` (a device
    /// tests only its own). Returns whether there is such a subscription.
    ///
    /// @throws String - Why the push service did not take the message.
    pub async fn send_test(&self, endpoint: &str, device_id: Option<&str>) -> Result<bool, String> {
        let found = self.subscriptions.read(|all| {
            all.iter()
                .find(|known| {
                    known.endpoint == endpoint && (device_id.is_none() || known.device_id.as_deref() == device_id)
                })
                .cloned()
        })?;
        let Some(subscription) = found else { return Ok(false) };
        let payload = serde_json::json!({
            "kind": "test",
            "title": "Notifications work",
            "body": "This device hears from your agent when a run fails or waits for you.",
        });
        match self.send(&subscription, payload.to_string().as_bytes(), "high").await {
            Delivery::Sent => Ok(true),
            Delivery::Gone => {
                let _gone = self.unsubscribe(endpoint, None);
                Err("the push service no longer knows this subscription: turn notifications on again".to_owned())
            }
            Delivery::Failed(why) => Err(why),
        }
    }

    /// How many subscriptions the server keeps.
    pub fn subscription_count(&self) -> usize {
        self.subscriptions.read(Vec::len).unwrap_or_default()
    }

    /// Send `payload` to one subscription.
    async fn send(&self, subscription: &Subscription, payload: &[u8], urgency: &'static str) -> Delivery {
        let request = (|| {
            let url = self.endpoint_url(&subscription.endpoint)?;
            let body = encrypt(payload, &subscription.keys()?, &self.random)?;
            let audience = url.origin().ascii_serialization();
            let authorization = self.key.authorization(&audience, &self.contact, now_ms() / 1000, &self.random)?;
            let client = self.client.manual_redirect_client(true).map_err(|e| PushError::Io(e.to_string()))?;
            Ok::<_, PushError>(
                client
                    .post(url)
                    .timeout(SEND_TIMEOUT)
                    .header("authorization", authorization)
                    .header("content-encoding", "aes128gcm")
                    .header("content-type", "application/octet-stream")
                    .header("ttl", TTL_SECS)
                    .header("urgency", urgency)
                    .body(body),
            )
        })();
        let request = match request {
            Ok(request) => request,
            Err(error) => return Delivery::Failed(error.to_string()),
        };
        match request.send().await {
            Ok(response) if response.status().is_success() => Delivery::Sent,
            Ok(response) if matches!(response.status().as_u16(), 404 | 410) => Delivery::Gone,
            Ok(response) => Delivery::Failed(format!("the push service answered {}", response.status())),
            Err(error) => Delivery::Failed(error.without_url().to_string()),
        }
    }
}

/// What the service worker of the monitoring page receives.
fn payload_of(notice: &Notice) -> Vec<u8> {
    serde_json::json!({
        "kind": notice.kind.name(),
        "title": notice.title,
        "body": notice.body,
        "run_id": notice.run_id,
        "workflow": notice.workflow,
    })
    .to_string()
    .into_bytes()
}

impl Notifier for WebPush {
    fn name(&self) -> &'static str {
        "web-push"
    }

    fn notify<'a>(&'a self, notice: &'a Notice) -> BoxFuture<'a, Result<(), NotifyError>> {
        Box::pin(async move {
            let subscriptions = self.subscriptions.read(Clone::clone).map_err(NotifyError)?;
            if subscriptions.is_empty() {
                return Ok(());
            }
            let payload = payload_of(notice);
            // A person is waiting on an approval; a failure can wait for the phone to wake.
            let urgency = match notice.kind {
                NoticeKind::ApprovalNeeded => "high",
                NoticeKind::RunFailed => "normal",
            };
            let sends = subscriptions.iter().map(|subscription| self.send(subscription, &payload, urgency));
            let outcomes = futures::future::join_all(sends).await;
            let mut sent = 0;
            let mut last_error = None;
            for (subscription, outcome) in subscriptions.iter().zip(outcomes) {
                match outcome {
                    Delivery::Sent => sent += 1,
                    Delivery::Gone => {
                        if let Err(error) = self.unsubscribe(&subscription.endpoint, None) {
                            eprintln!("warning: cannot remove a push subscription that is gone: {error}");
                        }
                    }
                    Delivery::Failed(error) => last_error = Some(error),
                }
            }
            match last_error {
                Some(error) if sent == 0 => Err(NotifyError(error)),
                Some(error) => {
                    eprintln!("warning: run {}: a push notification was not delivered: {error}", notice.run_id);
                    Ok(())
                }
                None => Ok(()),
            }
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn b64(text: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD.decode(text).unwrap()
    }

    // RFC 8291, appendix A.
    const UA_PUBLIC: &str = "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
    const AS_PUBLIC: &str = "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8";
    const AUTH_SECRET: &str = "BTBZMqHH6r4Tts7J_aSIgg";
    const SALT: &str = "DGv6ra1nlYgDCS1FRnbzlw";
    const ECDH_SECRET: &str = "kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs";
    const PLAINTEXT: &str = "V2hlbiBJIGdyb3cgdXAsIEkgd2FudCB0byBiZSBhIHdhdGVybWVsb24";

    #[test]
    fn the_message_of_rfc_8291_is_reproduced() {
        let (key, nonce) =
            content_keys(&b64(ECDH_SECRET), &b64(AUTH_SECRET), &b64(UA_PUBLIC), &b64(AS_PUBLIC), &b64(SALT)).unwrap();
        assert_eq!(URL_SAFE_NO_PAD.encode(key), "oIhVW04MRdy2XN9CiKLxTg");
        assert_eq!(URL_SAFE_NO_PAD.encode(nonce), "4h_95klXJ5E_qnoN");
        let record = seal_record(&key, nonce, &b64(PLAINTEXT)).unwrap();
        assert_eq!(
            URL_SAFE_NO_PAD.encode(&record),
            "8pfeW0KbunFT06SuDKoJH9Ql87S1QUrdirN6GcG7sFz1y1sqLgVi1VhjVkHsUoEsbI_0LpXMuGvnzQ"
        );
        let salt: [u8; 16] = b64(SALT).try_into().unwrap();
        let body = message_body(&salt, &b64(AS_PUBLIC), &record);
        assert_eq!(body.len(), 86 + record.len());
        assert_eq!(&body[86..], &record[..]);
        assert_eq!(
            URL_SAFE_NO_PAD.encode(&body[..86]),
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8"
        );
    }

    /// A browser: a key pair and an authentication secret.
    struct Browser {
        private: agreement::EphemeralPrivateKey,
        subscription: Subscription,
    }

    fn browser(endpoint: &str) -> Browser {
        let random = SystemRandom::new();
        let private = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &random).unwrap();
        let mut auth = [0_u8; 16];
        random.fill(&mut auth).unwrap();
        let subscription = Subscription {
            endpoint: endpoint.to_owned(),
            p256dh: URL_SAFE_NO_PAD.encode(private.compute_public_key().unwrap().as_ref()),
            auth: URL_SAFE_NO_PAD.encode(auth),
            device_id: None,
            created_ms: 0,
        };
        Browser { private, subscription }
    }

    /// What a browser does with a push message: derive the keys from its own
    /// private key and the sender's public key in the header, then open the record.
    fn decrypt(browser: Browser, body: &[u8]) -> Vec<u8> {
        let keys = browser.subscription.keys().unwrap();
        let (salt, rest) = body.split_at(16);
        assert_eq!(rest[..4], RECORD_SIZE.to_be_bytes());
        let key_len = usize::from(rest[4]);
        let (as_public, record) = rest[5..].split_at(key_len);
        let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, as_public);
        let (key, nonce) = agreement::agree_ephemeral(browser.private, &peer, |secret| {
            content_keys(secret, &keys.auth, &keys.public, as_public, salt).unwrap()
        })
        .unwrap();
        let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &key).unwrap());
        let mut record = record.to_vec();
        let opened =
            key.open_in_place(aead::Nonce::assume_unique_for_key(nonce), aead::Aad::empty(), &mut record).unwrap();
        let (delimiter, text) = opened.split_last().unwrap();
        assert_eq!(*delimiter, 2);
        text.to_vec()
    }

    #[test]
    fn a_browser_can_read_what_was_encrypted_for_it_and_nobody_else() {
        let random = SystemRandom::new();
        let receiver = browser("https://push.example.net/a");
        let body = encrypt(b"Approval needed", &receiver.subscription.keys().unwrap(), &random).unwrap();
        assert!(!body.windows(15).any(|window| window == b"Approval needed"));
        assert_eq!(decrypt(receiver, &body), b"Approval needed");
        // Every message has its own salt and key pair.
        let again = browser("https://push.example.net/a");
        let keys = again.subscription.keys().unwrap();
        assert_ne!(encrypt(b"x", &keys, &random).unwrap(), encrypt(b"x", &keys, &random).unwrap());
        assert!(matches!(encrypt(&[0; 4096], &keys, &random), Err(PushError::Invalid(_))));
    }

    #[test]
    fn the_vapid_token_is_signed_by_the_key_browsers_subscribed_with() {
        let random = SystemRandom::new();
        let key = VapidKey::open(None, &random).unwrap();
        let header = key.authorization("https://push.example.net", "mailto:ops@example.com", 1_000, &random).unwrap();
        let (token, public) = header.strip_prefix("vapid t=").unwrap().split_once(", k=").unwrap();
        assert_eq!(public, key.public);
        let mut parts = token.split('.');
        let (head, claims, signature) = (parts.next().unwrap(), parts.next().unwrap(), parts.next().unwrap());
        assert_eq!(b64(head), br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims: serde_json::Value = serde_json::from_slice(&b64(claims)).unwrap();
        assert_eq!(claims["aud"], "https://push.example.net");
        assert_eq!(claims["sub"], "mailto:ops@example.com");
        assert_eq!(claims["exp"], 1_000 + VAPID_LIFETIME_SECS);
        let verifier = ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, b64(public));
        let signed = token.rsplit_once('.').unwrap().0;
        verifier.verify(signed.as_bytes(), &b64(signature)).unwrap();
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("graphrt-push-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_key_and_the_subscriptions_survive_a_restart() {
        let dir = temp_dir("restart");
        let push = WebPush::open(Some(&crate::store::StoreLocation::Dir(dir.clone())), "mailto:ops@example.com", false)
            .unwrap();
        push.subscribe(browser("https://push.example.net/a").subscription).unwrap();
        let again =
            WebPush::open(Some(&crate::store::StoreLocation::Dir(dir.clone())), "mailto:ops@example.com", false)
                .unwrap();
        assert_eq!(again.public_key(), push.public_key());
        assert_eq!(again.subscription_count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.join(DIR_NAME).join(KEY_FILE)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subscriptions_are_checked_replaced_and_removed() {
        let push = WebPush::open(None, "mailto:ops@example.com", false).unwrap();
        let mut phone = browser("https://push.example.net/phone").subscription;
        phone.device_id = Some("dev-1".to_owned());
        push.subscribe(phone.clone()).unwrap();
        push.subscribe(phone.clone()).unwrap();
        assert_eq!(push.subscription_count(), 1, "the same endpoint is one subscription");
        // A device keeps its newest few.
        for n in 0..MAX_PER_DEVICE {
            let mut more = browser(&format!("https://push.example.net/{n}")).subscription;
            more.device_id = Some("dev-1".to_owned());
            push.subscribe(more).unwrap();
        }
        assert_eq!(push.subscription_count(), MAX_PER_DEVICE);
        // Another device cannot remove it; the key holder and its own device can.
        assert!(!push.unsubscribe("https://push.example.net/0", Some("dev-2")).unwrap());
        assert!(push.unsubscribe("https://push.example.net/0", Some("dev-1")).unwrap());
        assert!(push.unsubscribe("https://push.example.net/1", None).unwrap());
        push.forget_device("dev-1");
        assert_eq!(push.subscription_count(), 0);

        let bad_endpoints =
            ["http://push.example.net/a", "https://127.0.0.1/a", "https://user:pw@push.example.net/a", "not a url"];
        for endpoint in bad_endpoints {
            let refused = push.subscribe(browser(endpoint).subscription);
            assert!(matches!(refused, Err(PushError::Invalid(_))), "{endpoint}");
        }
        let mut bad_key = browser("https://push.example.net/a").subscription;
        bad_key.p256dh = URL_SAFE_NO_PAD.encode([4_u8; 20]);
        assert!(matches!(push.subscribe(bad_key), Err(PushError::Invalid(_))));
        let mut bad_auth = browser("https://push.example.net/a").subscription;
        bad_auth.auth = "short".to_owned();
        assert!(matches!(push.subscribe(bad_auth), Err(PushError::Invalid(_))));
    }

    #[test]
    fn the_contact_is_an_address() {
        for bad in ["", "ops@example.com", "mailto:ops", "http://example.com", "mailto:a b@example.com"] {
            assert!(WebPush::open(None, bad, false).is_err(), "{bad}");
        }
        assert!(WebPush::open(None, "https://example.com/contact", false).is_ok());
    }
}
