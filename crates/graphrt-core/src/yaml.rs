//! The one YAML entry point of graphrt: YAML text to a [`serde_json::Value`].
//!
//! Every YAML document graphrt reads (workflows, plugin manifests, bundles,
//! eval and knowledge configs, the gateway config) is parsed here and then
//! deserialized from the `Value`. `serde_saphyr`'s deserializer is generic over
//! the visitor, so deserializing each type straight from YAML compiles its whole
//! deserializer again per type and per crate (≈ 430 KiB of a gateway binary
//! before this module). Going through one non-generic function keeps a single
//! copy. Types then deserialize from `Value`, which the JSON inputs use anyway;
//! errors name the offending path ([`crate::value_path`]) and, with a
//! [`crate::source_map::SourceMap`], its line.

/// A YAML syntax error, with its location.
pub use serde_saphyr::Error;

/// Parses YAML text into a `Value`. With `strict_booleans`, only `true` and
/// `false` are booleans (YAML 1.2: `yes`, `no`, `on`, `off` stay strings).
///
/// Unquoted scalars become numbers, booleans or null as YAML resolves them, so
/// a string field given `123` must quote it (`"123"`).
///
/// # Errors
/// Syntax errors, with their location.
#[inline(never)]
pub fn to_value(text: &str, strict_booleans: bool) -> Result<serde_json::Value, Error> {
    let options = serde_saphyr::options! { strict_booleans: strict_booleans };
    serde_saphyr::from_str_with_options(text, options)
}

/// Parses YAML text into `T` through [`to_value`]. An error reads
/// `path (line L, column C): message`, or just the message for a syntax error
/// (which carries its own location).
///
/// # Errors
/// The syntax or deserialization error as text.
pub fn from_str<T: serde::de::DeserializeOwned>(text: &str, strict_booleans: bool) -> Result<T, String> {
    let value = to_value(text, strict_booleans).map_err(|error| error.to_string())?;
    T::deserialize(&value).map_err(|error| describe(text, &value, &error))
}

/// `path (line L, column C): message` for an error deserializing `value`, parsed from `text`.
#[inline(never)]
pub fn describe(text: &str, value: &serde_json::Value, error: &serde_json::Error) -> String {
    let path = crate::value_path::error_path(value, error);
    if path.is_root() {
        return error.to_string();
    }
    let prefer_key = error.to_string().starts_with("unknown field");
    match crate::source_map::SourceMap::from_text(text).locate(&path, prefer_key) {
        Some(span) => format!("{path} (line {}, column {}): {error}", span.line, span.column),
        None => format!("{path}: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_yaml_into_values() {
        let value = to_value("a: 1\nb: [x, true]\nc: yes\n", true).unwrap();
        assert_eq!(value, json!({ "a": 1, "b": ["x", true], "c": "yes" }));
        assert_eq!(to_value("c: yes\n", false).unwrap(), json!({ "c": true }));
        let error = to_value("a: [1\n", true).unwrap_err();
        assert!(error.location().is_some(), "{error}");
    }

    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Pool {
        max_active_runs: u32,
    }

    #[derive(Debug, serde::Deserialize)]
    #[allow(dead_code)]
    struct File {
        name: String,
        pool: Pool,
    }

    #[test]
    fn typed_errors_name_path_and_line() {
        let ok: File = from_str("name: shop\npool: { max_active_runs: 4 }\n", false).unwrap();
        assert_eq!(ok.pool.max_active_runs, 4);
        let error = from_str::<File>("name: shop\npool:\n  max_active_runs: many\n", false).unwrap_err();
        assert!(
            error.starts_with("pool.max_active_runs (line 3, column 20): invalid type: string \"many\""),
            "{error}"
        );
        let error = from_str::<File>("name: shop\npool:\n  max_active: 1\n", false).unwrap_err();
        assert!(error.starts_with("pool.max_active (line 3, column 3): unknown field `max_active`"), "{error}");
        assert!(from_str::<File>("name: [\n", false).unwrap_err().contains("line"));
    }
}
