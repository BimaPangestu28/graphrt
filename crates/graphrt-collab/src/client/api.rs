//! The HTTP side of the client: checking a server, signing in, workspaces,
//! invites, projects and versions.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use super::{ClientError, ClientResult};

/// What `GET /v1/info` says about a server.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ServerInfo {
    pub server: String,
    pub version: String,
    pub mode: String,
    pub public_url: String,
    pub sign_in: SignIn,
}

/// How people sign in on a server.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct SignIn {
    /// With an identity provider (in the browser).
    pub oidc: bool,
    #[serde(default)]
    pub provider: Option<String>,
    /// The identity providers, each a sign in button (servers before 0.6.4 list none:
    /// sign in without picking one).
    #[serde(default)]
    pub providers: Vec<SignInProvider>,
    /// With an invite code alone.
    pub invite_codes: bool,
}

/// An identity provider of a server.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct SignInProvider {
    /// What [`Api::login_url`] takes to pick it.
    pub id: String,
    /// The name on the button, e.g. `Google`.
    pub name: String,
}

/// Tokens of a sign in.
#[derive(Clone, Debug, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    /// Seconds the access token lasts.
    pub expires_in: u64,
    pub refresh_token: String,
    pub account: Value,
}

/// What redeeming an invite gives.
#[derive(Clone, Debug, Deserialize)]
pub struct Redeemed {
    pub workspace: Value,
    pub role: String,
    pub account: Value,
    /// For a new account made by the invite.
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub access_token: Option<String>,
}

/// A PKCE pair (RFC 7636, S256).
#[derive(Clone, Debug)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    /// A fresh pair.
    ///
    /// @throws ClientError::Internal - When the system has no randomness.
    pub fn new() -> ClientResult<Self> {
        let verifier = random_text()?;
        let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
        Ok(Self { challenge: b64url(digest.as_ref()), verifier })
    }
}

/// 256 random bits as base64url.
pub fn random_text() -> ClientResult<String> {
    use ring::rand::SecureRandom as _;
    let mut bytes = [0u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| ClientError::Internal("no system randomness".into()))?;
    Ok(b64url(&bytes))
}

fn b64url(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A collaboration server.
#[derive(Clone, Debug)]
pub struct Api {
    base: String,
    http: reqwest::Client,
}

/// Check a server address as people type it: `https://` (or `http://` to
/// this computer only), no credentials, no query.
///
/// @returns The address without a trailing slash.
/// @throws ClientError::Address - With a reason people can act on.
pub fn normalize_url(text: &str) -> ClientResult<String> {
    let text = text.trim().trim_end_matches('/');
    let with_scheme = if text.contains("://") { text.to_owned() } else { format!("https://{text}") };
    let url =
        reqwest::Url::parse(&with_scheme).map_err(|_| ClientError::Address("this is not a server address".into()))?;
    let host = url.host_str().ok_or_else(|| ClientError::Address("the address has no host".into()))?;
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return Err(ClientError::Address("the address must not hold a user name, a query or a #".into()));
    }
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1");
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        "http" => {
            return Err(ClientError::Address(
                "plain http is only for this computer (localhost): use https:// for a server".into(),
            ));
        }
        _ => return Err(ClientError::Address("the address must start with https://".into())),
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

impl Api {
    /// A client of the server at `url` (see [`normalize_url`]).
    ///
    /// @throws ClientError - `Address` for an unusable address, `Internal` when the HTTP client cannot be built.
    pub fn new(url: &str) -> ClientResult<Self> {
        let base = normalize_url(url)?;
        super::install_tls_provider();
        let http = reqwest::Client::builder()
            .user_agent(concat!("graphrt-collab/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| ClientError::Internal(error.to_string()))?;
        Ok(Self { base, http })
    }

    /// The server's address.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Check that a collaboration server answers at the address.
    ///
    /// @throws ClientError - `Unreachable`, `Tls` (an invalid certificate), `NotCollab` or `Http`.
    pub async fn info(&self) -> ClientResult<ServerInfo> {
        let response = self.http.get(format!("{}/v1/info", self.base)).send().await.map_err(classify)?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(if status == 404 {
                ClientError::NotCollab
            } else {
                ClientError::Http { status, message: String::new() }
            });
        }
        let info: ServerInfo = response.json().await.map_err(|_| ClientError::NotCollab)?;
        if info.server != "graphrt-collab" {
            return Err(ClientError::NotCollab);
        }
        Ok(info)
    }

    /// Where to send the browser to sign in, for an app listening on `redirect_uri` (loopback).
    ///
    /// @throws ClientError::Address - When the address cannot be built.
    /// `provider` is the id of one of [`SignIn::providers`], `None` for the server's first.
    pub fn login_url(
        &self,
        redirect_uri: &str,
        state: &str,
        pkce: &Pkce,
        device: &str,
        provider: Option<&str>,
    ) -> ClientResult<String> {
        let mut params = vec![
            ("redirect_uri", redirect_uri),
            ("state", state),
            ("code_challenge", pkce.challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("device", device),
        ];
        if let Some(provider) = provider {
            params.push(("provider", provider));
        }
        reqwest::Url::parse_with_params(&format!("{}/v1/auth/login", self.base), params)
            .map(String::from)
            .map_err(|error| ClientError::Address(error.to_string()))
    }

    /// Trade the code of a finished browser sign in for tokens.
    pub async fn exchange_code(&self, code: &str, pkce: &Pkce, redirect_uri: &str) -> ClientResult<Tokens> {
        let body = json!({
            "grant_type": "authorization_code",
            "code": code,
            "code_verifier": pkce.verifier,
            "redirect_uri": redirect_uri,
        });
        self.send_as(self.http.post(format!("{}/v1/auth/token", self.base)).json(&body)).await
    }

    /// Trade a refresh token for new tokens (the old refresh token stops working).
    pub async fn refresh(&self, refresh_token: &str) -> ClientResult<Tokens> {
        let body = json!({ "grant_type": "refresh_token", "refresh_token": refresh_token });
        self.send_as(self.http.post(format!("{}/v1/auth/token", self.base)).json(&body)).await
    }

    /// End a sign in.
    pub async fn logout(&self, refresh_token: &str) -> ClientResult<()> {
        let body = json!({ "refresh_token": refresh_token });
        self.send(self.http.post(format!("{}/v1/auth/logout", self.base)).json(&body)).await.map(|_| ())
    }

    /// A workspace scoped access token, for sync sessions.
    ///
    /// @returns The token and the seconds it lasts.
    pub async fn scoped_token(&self, access: &str, workspace: &str) -> ClientResult<(String, u64)> {
        let answer = self.post(access, "/v1/auth/scoped", &json!({ "workspace": workspace })).await?;
        let token = answer
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| ClientError::Internal("no token".into()))?;
        Ok((token.to_owned(), answer.get("expires_in").and_then(Value::as_u64).unwrap_or(600)))
    }

    /// What an invite (a code or an invite link) is for.
    pub async fn preview_invite(&self, code: &str) -> ClientResult<Value> {
        self.send(self.http.post(format!("{}/v1/invites/preview", self.base)).json(&json!({ "code": code }))).await
    }

    /// Join with an invite. Signed in (`access`): the account joins. Signed out:
    /// a new account named `name` is made (on servers that allow it).
    pub async fn redeem_invite(
        &self,
        code: &str,
        access: Option<&str>,
        name: Option<&str>,
        device: &str,
    ) -> ClientResult<Redeemed> {
        let mut request = self
            .http
            .post(format!("{}/v1/invites/redeem", self.base))
            .json(&json!({ "code": code, "name": name, "device": device }));
        if let Some(access) = access {
            request = request.bearer_auth(access);
        }
        self.send_as(request).await
    }

    /// `GET path` as the holder of `access`.
    pub async fn get(&self, access: &str, path: &str) -> ClientResult<Value> {
        self.send(self.http.get(format!("{}{path}", self.base)).bearer_auth(access)).await
    }

    /// `POST path` with a JSON body.
    pub async fn post(&self, access: &str, path: &str, body: &Value) -> ClientResult<Value> {
        self.send(self.http.post(format!("{}{path}", self.base)).bearer_auth(access).json(body)).await
    }

    /// `PUT path` with a JSON body.
    pub async fn put(&self, access: &str, path: &str, body: &Value) -> ClientResult<Value> {
        self.send(self.http.put(format!("{}{path}", self.base)).bearer_auth(access).json(body)).await
    }

    /// `PATCH path` with a JSON body.
    pub async fn patch(&self, access: &str, path: &str, body: &Value) -> ClientResult<Value> {
        self.send(self.http.patch(format!("{}{path}", self.base)).bearer_auth(access).json(body)).await
    }

    /// `DELETE path`.
    pub async fn delete(&self, access: &str, path: &str) -> ClientResult<Value> {
        self.send(self.http.delete(format!("{}{path}", self.base)).bearer_auth(access)).await
    }

    /// The WebSocket address of a project's sync session.
    pub fn sync_url(&self, workspace: &str, project: &str) -> String {
        let ws_base = match self.base.strip_prefix("https://") {
            Some(rest) => format!("wss://{rest}"),
            None => format!("ws://{}", self.base.trim_start_matches("http://")),
        };
        format!("{ws_base}/v1/workspaces/{workspace}/projects/{project}/sync")
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> ClientResult<Value> {
        self.send_as(request).await
    }

    async fn send_as<T: serde::de::DeserializeOwned>(&self, request: reqwest::RequestBuilder) -> ClientResult<T> {
        let response = request.send().await.map_err(classify)?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await.map_err(classify)?;
        if !(200..300).contains(&status) {
            let message = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_default();
            return Err(ClientError::Http { status, message });
        }
        serde_json::from_slice(&bytes).map_err(|error| ClientError::Internal(format!("unexpected answer: {error}")))
    }
}

/// Tell a certificate problem from an unreachable server.
fn classify(error: reqwest::Error) -> ClientError {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    while let Some(current) = source {
        if current.downcast_ref::<rustls::Error>().is_some() {
            return ClientError::Tls(current.to_string());
        }
        let text = current.to_string().to_ascii_lowercase();
        if text.contains("certificate") || text.contains("invalid peer") || text.contains("tls handshake") {
            return ClientError::Tls(current.to_string());
        }
        source = current.source();
    }
    if error.is_timeout() || error.is_connect() {
        return ClientError::Unreachable(error.to_string());
    }
    ClientError::Unreachable(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_addresses() {
        assert_eq!(normalize_url("collab.example.com/").unwrap(), "https://collab.example.com");
        assert_eq!(normalize_url("https://collab.example.com:8443").unwrap(), "https://collab.example.com:8443");
        assert_eq!(normalize_url("http://localhost:8790").unwrap(), "http://localhost:8790");
        assert_eq!(normalize_url("http://127.0.0.1:8790/").unwrap(), "http://127.0.0.1:8790");
        assert!(matches!(normalize_url("http://collab.example.com"), Err(ClientError::Address(_))));
        assert!(normalize_url("ftp://x.example").is_err());
        assert!(normalize_url("https://user:pw@x.example").is_err());
        assert!(normalize_url("https://x.example/?a=1").is_err());
        assert!(normalize_url("").is_err());
    }

    #[test]
    fn sync_urls_follow_the_scheme() {
        let api = Api::new("https://collab.example.com/team").unwrap();
        assert_eq!(api.sync_url("ws_1", "pr_2"), "wss://collab.example.com/team/v1/workspaces/ws_1/projects/pr_2/sync");
        let api = Api::new("http://127.0.0.1:9").unwrap();
        assert_eq!(api.sync_url("w", "p"), "ws://127.0.0.1:9/v1/workspaces/w/projects/p/sync");
    }

    #[test]
    fn pkce_pairs() {
        let pkce = Pkce::new().unwrap();
        assert_eq!(pkce.verifier.len(), 43);
        assert_eq!(pkce.challenge.len(), 43);
        assert_ne!(Pkce::new().unwrap().verifier, pkce.verifier);
    }
}
