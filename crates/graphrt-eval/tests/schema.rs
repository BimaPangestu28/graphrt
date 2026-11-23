//! The published eval file schema matches the generator, and every example
//! eval file validates against it and loads.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};

use graphrt_eval::EvalSuite;
use graphrt_eval::schema::eval_file_schema;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn eval_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.to_string_lossy().ends_with(".eval.yaml") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(&repo().join("examples"), &mut out);
    out.sort();
    out
}

#[test]
fn checked_in_schema_is_current() {
    let generated = format!("{}\n", serde_json::to_string_pretty(&eval_file_schema()).unwrap());
    let checked_in = std::fs::read_to_string(repo().join("schema/eval-v1.schema.json")).expect("schema file");
    assert!(
        generated == checked_in,
        "schema/eval-v1.schema.json is stale; regenerate it with\n  \
         cargo run -q -p graphrt-eval --example eval-schema > schema/eval-v1.schema.json"
    );
}

#[test]
fn example_eval_files_validate_and_load() {
    let validator = jsonschema::draft202012::new(&eval_file_schema()).expect("valid schema");
    let files = eval_files();
    assert!(files.len() >= 4, "{files:?}");
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let value = graphrt_core::yaml::to_value(&text, true).unwrap();
        let errors: Vec<String> = validator.iter_errors(&value).map(|e| e.to_string()).collect();
        assert!(errors.is_empty(), "{}: {errors:?}", file.display());
        let suite = EvalSuite::load(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        assert!(!suite.cases.is_empty(), "{}", file.display());
        assert!(suite.workflow.is_file(), "{}: missing workflow", file.display());
    }
}

#[test]
fn schema_rejects_typos() {
    let validator = jsonschema::draft202012::new(&eval_file_schema()).expect("valid schema");
    let bad = serde_json::json!({"workflow": "w.yaml", "test": []});
    assert!(!validator.is_valid(&bad));
    let bad = serde_json::json!({"workflow": "w.yaml", "tests": [{"asert": []}]});
    assert!(!validator.is_valid(&bad));
    let ok = serde_json::json!({"workflow": "w.yaml", "x-owner": "me", "tests": [{"assert": [{"type": "my-custom"}]}]});
    assert!(validator.is_valid(&ok));
}
