//! Download links for files a run produced, for a chat that cannot take the
//! file itself (a Teams channel or group chat, a file above what a channel
//! uploads). Only with `--public-url`, so the link reaches the server.
//!
//! A link is `<public url>/v1/channels/{name}/files/{token}`: the token is 128
//! random bits, names one workspace file for one channel, and expires after
//! [`LINK_LIFETIME`]. Anyone with the link can download the file while it is
//! valid, as with a shared link in any chat. Links are kept in memory and end
//! with the process.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::{Bot, Channels, locked};
use crate::serve::{AppState, error_response};

/// How long a link works.
pub const LINK_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Links remembered at once. The oldest go first.
const MAX_LINKS: usize = 10_000;

/// One shared file.
#[derive(Clone, Debug)]
struct Link {
    channel: String,
    /// The workspace reference of the file (`File::reference`).
    reference: String,
    name: String,
    mime_type: String,
    created: Instant,
}

/// The links of a server.
#[derive(Debug, Default)]
pub(super) struct Links {
    links: Mutex<HashMap<String, Link>>,
}

fn random_token() -> Option<String> {
    let mut bytes = [0_u8; 16];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes).ok()?;
    Some(crate::hex::to_hex(&bytes))
}

/// A file name for `Content-Disposition`: letters, digits, `.`, `-`, `_` and spaces.
fn disposition_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') { c } else { '_' })
        .take(120)
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.');
    if cleaned.is_empty() { "file".to_owned() } else { cleaned.to_owned() }
}

impl Links {
    /// A new link to `file` for the channel `bot`, or `None` without a public
    /// address or a workspace reference.
    pub(super) fn share(
        &self,
        public_url: Option<&str>,
        bot: &Bot,
        file: &graphrt_core::value::File,
    ) -> Option<String> {
        let public_url = public_url?;
        let reference = file.reference.clone()?;
        let token = random_token()?;
        let mut links = locked(&self.links);
        links.retain(|_, link| link.created.elapsed() < LINK_LIFETIME);
        if links.len() >= MAX_LINKS
            && let Some(oldest) = links.iter().min_by_key(|(_, link)| link.created).map(|(token, _)| token.clone())
        {
            links.remove(&oldest);
        }
        links.insert(
            token.clone(),
            Link {
                channel: bot.name().to_owned(),
                reference,
                name: file.name.clone(),
                mime_type: file.mime_type.clone().unwrap_or_else(|| "application/octet-stream".to_owned()),
                created: Instant::now(),
            },
        );
        Some(format!("{public_url}/v1/channels/{}/files/{token}", bot.name()))
    }

    fn find(&self, channel: &str, token: &str) -> Option<Link> {
        let links = locked(&self.links);
        links.get(token).filter(|link| link.channel == channel && link.created.elapsed() < LINK_LIFETIME).cloned()
    }
}

impl Channels {
    /// The bytes of a shared file (`GET /v1/channels/{name}/files/{token}`).
    pub(super) async fn shared_file(&self, state: &AppState, bot: &Bot, token: &str) -> Response {
        let gone = || error_response(StatusCode::NOT_FOUND, "this link does not exist or has expired");
        let Some(link) = self.links.find(bot.name(), token) else { return gone() };
        let Some(served) = state.workspace() else { return gone() };
        let Some((view, path)) = served.workspace.locate_reference(&link.reference) else { return gone() };
        let bytes = match view.read_all(&path, bot.config.max_file_bytes).await {
            Ok(bytes) => bytes,
            Err(error) => {
                eprintln!("warning: channel `{}`: a shared file was not read: {error}", bot.name());
                return gone();
            }
        };
        let disposition = format!("attachment; filename=\"{}\"", disposition_name(&link.name));
        let mut response = bytes.into_response();
        let headers = response.headers_mut();
        let content_type =
            HeaderValue::from_str(&link.mime_type).unwrap_or(HeaderValue::from_static("application/octet-stream"));
        headers.insert(header::CONTENT_TYPE, content_type);
        if let Ok(value) = HeaderValue::from_str(&disposition) {
            headers.insert(header::CONTENT_DISPOSITION, value);
        }
        headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_in_the_header_cannot_break_it() {
        assert_eq!(disposition_name("Faktur \"Juli\".pdf"), "Faktur _Juli_.pdf");
        assert_eq!(disposition_name("../x\r\ny"), "_x__y");
        assert_eq!(disposition_name(".."), "file");
    }

    #[test]
    fn tokens_are_long_and_never_repeat() {
        let (first, second) = (random_token().unwrap_or_default(), random_token().unwrap_or_default());
        assert_eq!(first.len(), 32);
        assert_ne!(first, second);
    }
}
