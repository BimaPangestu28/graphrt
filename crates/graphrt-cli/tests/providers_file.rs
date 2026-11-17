//! Providers-file credential rules enforced at startup.

#![allow(clippy::expect_used)]

use std::path::PathBuf;
use std::process::{Command, Output};

fn example(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples").join(name)
}

fn providers_file(name: &str, body: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphrt-providers-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write providers file");
    path
}

fn run_with(providers: &PathBuf) -> Output {
    Command::new(env!("CARGO_BIN_EXE_graphrt"))
        .args(["run", example("hello.yaml").to_str().expect("utf8"), "--input", "name=Ada", "--providers"])
        .arg(providers)
        .arg("--no-default-plugins")
        .output()
        .expect("spawn graphrt")
}

#[test]
fn external_credentials_are_rejected_at_startup() {
    let file = providers_file("external.json", r#"{"credentials": {"openai": {"source": "external"}}}"#);
    let out = run_with(&file);
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("host app"));
}

#[test]
fn typesafe_credentials_entry_is_accepted() {
    let file = providers_file(
        "typesafe.json",
        r#"{"credentials": {"typesafe": {"source": "env", "var": "GRAPHRT_TEST_TYPESAFE_KEY"}}}"#,
    );
    let out = run_with(&file);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}
