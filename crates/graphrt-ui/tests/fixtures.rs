//! The shared fixtures (`fixtures/valid`, `fixtures/invalid`) against the validator and the
//! JSON Schemas. The catalog plugin and hosts reuse the same files for their own checks.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};

use graphrt_ui::{UiKind, schema, validate};
use serde_json::Value;

fn fixtures(kind: &str) -> Vec<(PathBuf, Value)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(kind);
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no fixtures in {}", dir.display());
    files
        .into_iter()
        .map(|path| {
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            (path, value)
        })
        .collect()
}

fn schema_validator(kind: UiKind) -> jsonschema::Validator {
    let schema: Value = serde_json::from_str(schema(kind)).expect("schema is JSON");
    jsonschema::validator_for(&schema).unwrap_or_else(|e| panic!("schema of {kind}: {e}"))
}

#[test]
fn valid_fixtures_pass_the_validator_and_their_schema() {
    for (path, block) in fixtures("valid") {
        let validated = validate(&block).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let validator = schema_validator(validated.kind());
        let errors: Vec<String> =
            validator.iter_errors(&block).map(|e| format!("{e} at {}", e.instance_path())).collect();
        assert!(errors.is_empty(), "{} fails its schema: {errors:?}", path.display());
    }
}

#[test]
fn invalid_fixtures_fail_with_the_expected_message() {
    for (path, fixture) in fixtures("invalid") {
        let expected = fixture["error"].as_str().unwrap_or_else(|| panic!("{}: no `error`", path.display()));
        match validate(&fixture["block"]) {
            Ok(_) => panic!("{} was accepted", path.display()),
            Err(error) => {
                assert!(error.to_string().contains(expected), "{}: `{error}` lacks `{expected}`", path.display())
            }
        }
    }
}

#[test]
fn every_kind_has_a_valid_fixture() {
    let kinds: Vec<UiKind> =
        fixtures("valid").iter().filter_map(|(_, block)| validate(block).ok()).map(|b| b.kind()).collect();
    for kind in UiKind::ALL {
        assert!(kinds.contains(&kind), "no valid fixture for {kind}");
    }
}
