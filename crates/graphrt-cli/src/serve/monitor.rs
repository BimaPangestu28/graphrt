//! The monitoring page of `graphrt serve` (`--monitor`, cargo feature
//! `web-monitor`): runs, what a run did, and the approvals inbox, for a phone.
//!
//! | Method | Path | Answer |
//! | --- | --- | --- |
//! | GET | `/monitor/` | the page |
//! | GET | `/monitor/monitor.js` | its script (client, UI blocks renderer and the views) |
//! | GET | `/monitor/sw.js` | its service worker: shows push notifications, keeps the page installable |
//! | GET | `/monitor/manifest.webmanifest` | the web app manifest, which makes the page installable |
//! | GET | `/monitor/icon-192.png`, `icon-512.png`, `icon-maskable.png`, `apple-touch-icon.png` | its icons |
//! | GET | `/v1/monitor/config` | what the page offers (approvals and whether they expire, pairing, push); public |
//!
//! The page is public like a login page: it holds nothing. Everything it shows
//! comes from the API with the API key or a device token ([`crate::devices`])
//! the person enters or pairs.
//!
//! The files are built from `web/` and embedded in the binary
//! (`crates/graphrt-cli/assets/monitor`). They load nothing from other hosts:
//! the page's `Content-Security-Policy` allows scripts, workers and
//! connections of its own origin only, and no framing.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::header::LOCATION;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use super::assets::{Asset, HTML_TYPE, SCRIPT_TYPE};
use super::errors::error_response;
use super::state::AppState;

const PAGE: &[u8] = include_bytes!("../../assets/monitor/index.html");
const PAGE_SCRIPT: &[u8] = include_bytes!("../../assets/monitor/monitor.js");
const WORKER: &[u8] = include_bytes!("../../assets/monitor/sw.js");
const MANIFEST: &[u8] = include_bytes!("../../assets/monitor/manifest.webmanifest");
const ICON_192: &[u8] = include_bytes!("../../assets/monitor/icon-192.png");
const ICON_512: &[u8] = include_bytes!("../../assets/monitor/icon-512.png");
const ICON_MASKABLE: &[u8] = include_bytes!("../../assets/monitor/icon-maskable.png");
const ICON_APPLE: &[u8] = include_bytes!("../../assets/monitor/apple-touch-icon.png");

/// The policy of the page: scripts, styles, workers and connections of its own
/// origin only, images also from `https` and `data:` (what UI blocks may
/// show), and no framing: the page holds a credential.
const PAGE_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data: https:; \
     connect-src 'self'; manifest-src 'self'; worker-src 'self'; base-uri 'none'; form-action 'none'; \
     frame-ancestors 'none'";

/// The monitoring page of a server.
#[derive(Debug)]
pub struct Monitor {
    page: Asset,
    script: Asset,
    worker: Asset,
    manifest: Asset,
    icon_192: Asset,
    icon_512: Asset,
    icon_maskable: Asset,
    icon_apple: Asset,
    has_api_key: bool,
}

impl Monitor {
    /// The monitoring page.
    ///
    /// @param has_api_key - Whether the server has an API key (without one the page asks for nothing).
    pub fn new(has_api_key: bool) -> Self {
        Self {
            page: Asset::new(PAGE, HTML_TYPE),
            script: Asset::new(PAGE_SCRIPT, SCRIPT_TYPE),
            worker: Asset::new(WORKER, SCRIPT_TYPE),
            manifest: Asset::new(MANIFEST, "application/manifest+json"),
            icon_192: Asset::new(ICON_192, "image/png"),
            icon_512: Asset::new(ICON_512, "image/png"),
            icon_maskable: Asset::new(ICON_MASKABLE, "image/png"),
            icon_apple: Asset::new(ICON_APPLE, "image/png"),
            has_api_key,
        }
    }

    /// One line for the startup log.
    pub fn describe(&self) -> String {
        "GET  /monitor/ (monitoring page: runs and approvals, installable on a phone)".to_owned()
    }

    /// What `GET /v1/monitor/config` answers.
    fn config(&self, state: &AppState) -> serde_json::Value {
        #[cfg(feature = "web-push")]
        let push = state.push.is_some();
        #[cfg(not(feature = "web-push"))]
        let push = false;
        serde_json::json!({
            // Without an API key nothing needs to be entered.
            "access": if self.has_api_key { "key" } else { "open" },
            "devices": state.devices.is_some(),
            "approvals": state.approvals.is_some(),
            // Whether the server carries out `on_expire` (`--expire-approvals`),
            // so the page can say what happens at a request's time limit.
            "expire_approvals": state.approvals.is_some_and(|approvals| approvals.expire),
            "push": push,
        })
    }
}

/// The monitoring routes. The page and its config are public: everything else
/// the page calls is the ordinary API.
pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/monitor", get(redirect_to_page))
        .route("/monitor/", get(page))
        .route("/monitor/{file}", get(file))
        .route("/v1/monitor/config", get(config))
}

fn monitor_of(state: &AppState) -> Result<&Monitor, Response> {
    state.monitor.as_deref().ok_or_else(|| error_response(StatusCode::NOT_FOUND, "this server has no monitoring page"))
}

/// `GET /monitor`: to `/monitor/`, relative so it also works below a path prefix of a proxy.
async fn redirect_to_page() -> Response {
    (StatusCode::PERMANENT_REDIRECT, [(LOCATION, HeaderValue::from_static("monitor/"))]).into_response()
}

async fn page(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    match monitor_of(&state) {
        Ok(monitor) => monitor.page.page_response(&headers, &HeaderValue::from_static(PAGE_POLICY)),
        Err(refused) => refused,
    }
}

async fn file(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(file): axum::extract::Path<String>,
    headers: HeaderMap,
) -> Response {
    let monitor = match monitor_of(&state) {
        Ok(monitor) => monitor,
        Err(refused) => return refused,
    };
    let asset = match file.as_str() {
        "monitor.js" => &monitor.script,
        "sw.js" => &monitor.worker,
        "manifest.webmanifest" => &monitor.manifest,
        "icon-192.png" => &monitor.icon_192,
        "icon-512.png" => &monitor.icon_512,
        "icon-maskable.png" => &monitor.icon_maskable,
        "apple-touch-icon.png" => &monitor.icon_apple,
        _ => return error_response(StatusCode::NOT_FOUND, "no such file"),
    };
    asset.response(&headers)
}

/// `GET /v1/monitor/config`.
async fn config(State(state): State<Arc<AppState>>) -> Response {
    match monitor_of(&state) {
        Ok(monitor) => Json(monitor.config(&state)).into_response(),
        Err(refused) => refused,
    }
}
