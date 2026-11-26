//! Field readers shared by the kind validators: each takes the JSON Pointer of the object
//! it reads, so every error says where it is.

use serde_json::{Map, Value as Json};

use crate::UiError;

pub(crate) type Fields = Map<String, Json>;

/// Longest title, label or other short text, in characters.
pub(crate) const MAX_SHORT_TEXT: usize = 300;
/// Longest paragraph text, in characters.
pub(crate) const MAX_LONG_TEXT: usize = 10_000;
/// Longest URL, in characters (data URLs of small images fit; larger images do not).
pub(crate) const MAX_URL: usize = 200 * 1024;
/// Raster image types a `data:` URL may hold (no SVG: it can carry scripts).
const DATA_IMAGE_PREFIXES: &[&str] =
    &["data:image/png;base64,", "data:image/jpeg;base64,", "data:image/gif;base64,", "data:image/webp;base64,"];

/// Escapes `~` and `/` for a JSON Pointer segment.
pub(crate) fn escape_pointer(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

pub(crate) fn child(path: &str, segment: &str) -> String {
    format!("{path}/{}", escape_pointer(segment))
}

pub(crate) fn index(path: &str, i: usize) -> String {
    format!("{path}/{i}")
}

pub(crate) fn object<'a>(value: &'a Json, path: &str) -> Result<&'a Fields, UiError> {
    value.as_object().ok_or_else(|| UiError::new(path, "must be an object"))
}

/// Rejects fields outside `allowed`, so typos and unsupported options fail loudly.
pub(crate) fn only_known(fields: &Fields, path: &str, allowed: &[&str]) -> Result<(), UiError> {
    match fields.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => {
            Err(UiError::new(&child(path, key), format!("unknown field `{key}` (allowed: {})", allowed.join(", "))))
        }
        None => Ok(()),
    }
}

fn text_of<'a>(value: &'a Json, path: &str, max_chars: usize) -> Result<&'a str, UiError> {
    let text = value.as_str().ok_or_else(|| UiError::new(path, "must be a string"))?;
    let chars = text.chars().count();
    if chars > max_chars {
        return Err(UiError::new(path, format!("is {chars} characters long; at most {max_chars} are allowed")));
    }
    Ok(text)
}

pub(crate) fn optional_text<'a>(
    fields: &'a Fields,
    key: &str,
    path: &str,
    max_chars: usize,
) -> Result<Option<&'a str>, UiError> {
    match fields.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(value) => text_of(value, &child(path, key), max_chars).map(Some),
    }
}

pub(crate) fn required_text<'a>(
    fields: &'a Fields,
    key: &str,
    path: &str,
    max_chars: usize,
) -> Result<&'a str, UiError> {
    let value = fields.get(key).ok_or_else(|| UiError::new(path, format!("`{key}` is required")))?;
    text_of(value, &child(path, key), max_chars)
}

/// A required non-empty string such as an id or a key.
pub(crate) fn required_name<'a>(fields: &'a Fields, key: &str, path: &str) -> Result<&'a str, UiError> {
    let name = required_text(fields, key, path, MAX_SHORT_TEXT)?;
    if name.trim().is_empty() {
        return Err(UiError::new(&child(path, key), "must not be empty"));
    }
    Ok(name)
}

pub(crate) fn optional_bool(fields: &Fields, key: &str, path: &str) -> Result<Option<bool>, UiError> {
    match fields.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(UiError::new(&child(path, key), "must be true or false")),
    }
}

pub(crate) fn optional_number(fields: &Fields, key: &str, path: &str) -> Result<Option<f64>, UiError> {
    match fields.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Number(n)) => Ok(n.as_f64()),
        Some(_) => Err(UiError::new(&child(path, key), "must be a number")),
    }
}

/// One of `choices`, compared without regard to case (Adaptive Cards enums are case-insensitive).
pub(crate) fn optional_choice<'a>(
    fields: &'a Fields,
    key: &str,
    path: &str,
    choices: &[&str],
) -> Result<Option<&'a str>, UiError> {
    let Some(value) = optional_text(fields, key, path, MAX_SHORT_TEXT)? else { return Ok(None) };
    if choices.iter().any(|choice| choice.eq_ignore_ascii_case(value)) {
        Ok(Some(value))
    } else {
        Err(UiError::new(&child(path, key), format!("`{value}` is not one of: {}", choices.join(", "))))
    }
}

/// An array of `min..=max` items.
pub(crate) fn array<'a>(
    fields: &'a Fields,
    key: &str,
    path: &str,
    min: usize,
    max: usize,
) -> Result<&'a [Json], UiError> {
    let items_path = child(path, key);
    let items = match fields.get(key) {
        None | Some(Json::Null) if min == 0 => return Ok(&[]),
        None => return Err(UiError::new(path, format!("`{key}` is required"))),
        Some(Json::Array(items)) => items,
        Some(_) => return Err(UiError::new(&items_path, "must be an array")),
    };
    if items.len() < min {
        return Err(UiError::new(&items_path, format!("needs at least {min} item(s)")));
    }
    if items.len() > max {
        return Err(UiError::new(&items_path, format!("has {} items; at most {max} are allowed", items.len())));
    }
    Ok(items)
}

/// ISO 4217 code such as `IDR` or `USD`.
pub(crate) fn optional_currency(fields: &Fields, key: &str, path: &str) -> Result<(), UiError> {
    let Some(code) = optional_text(fields, key, path, MAX_SHORT_TEXT)? else { return Ok(()) };
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err(UiError::new(&child(path, key), "must be a 3-letter ISO 4217 code such as IDR or USD"));
    }
    Ok(())
}

/// Image sources hosts may show: `data:` PNG/JPEG/GIF/WebP, or `https` (hosts may still refuse to fetch it).
pub(crate) fn check_image_url(url: &str, path: &str) -> Result<(), UiError> {
    if url.chars().count() > MAX_URL {
        return Err(UiError::new(path, "the image URL is too long; use a smaller image"));
    }
    let lower = url.get(..32).unwrap_or(url).to_ascii_lowercase();
    if DATA_IMAGE_PREFIXES.iter().any(|prefix| lower.starts_with(prefix)) || is_https(url) {
        Ok(())
    } else {
        Err(UiError::new(path, "images must be https URLs or data:image/(png|jpeg|gif|webp);base64 URLs"))
    }
}

/// Link targets hosts may open: `https` and `mailto`.
pub(crate) fn check_link_url(url: &str, path: &str) -> Result<(), UiError> {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    if url.chars().count() <= 2048 && (is_https(url) || (lower.starts_with("mailto:") && url.len() > 7)) {
        Ok(())
    } else {
        Err(UiError::new(path, "links must be https or mailto URLs"))
    }
}

/// `https://<host>...` with a host and no whitespace or control characters.
fn is_https(url: &str) -> bool {
    let Some(rest) = url.get(..8).filter(|scheme| scheme.eq_ignore_ascii_case("https://")).map(|_| &url[8..]) else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    !host.is_empty() && !host.contains('@') && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_policy() {
        assert!(check_image_url("data:image/png;base64,iVBORw0KGgo=", "").is_ok());
        assert!(check_image_url("https://example.com/a.png", "").is_ok());
        assert!(check_image_url("data:image/svg+xml;base64,PHN2Zz4=", "").is_err());
        assert!(check_image_url("http://example.com/a.png", "").is_err());
        assert!(check_image_url("javascript:alert(1)", "").is_err());
        assert!(check_image_url("https://user@evil.example/a.png", "").is_err());
        assert!(check_link_url("mailto:sales@example.com", "").is_ok());
        assert!(check_link_url("HTTPS://example.com", "").is_ok());
        assert!(check_link_url("https://", "").is_err());
        assert!(check_link_url("file:///etc/passwd", "").is_err());
        assert!(check_link_url("https://example.com/a b", "").is_err());
    }
}
