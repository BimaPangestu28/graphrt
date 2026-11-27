//! `graphrt plugin install <name>` and `graphrt plugin search` against a local
//! catalog: `index.json` and tarballs are written to a scratch directory and
//! referenced with `file://` URLs, so no test touches the network. Every index
//! is signed with a throwaway minisign key that the tests name as the trusted
//! catalog key (`$GRAPHRT_PLUGIN_INDEX_KEY`), unless a test says otherwise.
#![cfg(feature = "plugins")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

use sha2::{Digest, Sha256};

/// A scratch directory unique to one test.
fn scratch(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphrt-catalog-cli-{}-{test_name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Run `graphrt` with an isolated HOME, plugin path and catalog index.
fn graphrt(home: &Path, args: &[&str]) -> Output {
    graphrt_with_env(home, args, &[])
}

/// The catalog key variable. Tests set it to the test key by default; pass it
/// as `""` in `env` to run with the built-in graphrt-plugins key instead.
const INDEX_KEY_ENV: &str = "GRAPHRT_PLUGIN_INDEX_KEY";

fn graphrt_with_env(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphrt"));
    command.args(args).env("HOME", home).env_remove("GRAPHRT_PLUGIN_PATH").env_remove("GRAPHRT_PLUGIN_INDEX");
    command.env("USERPROFILE", home).env(INDEX_KEY_ENV, test_public_key());
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("spawn graphrt")
}

/// The key the test catalogs are signed with, made once per test process.
fn test_key_pair() -> &'static minisign::KeyPair {
    static KEY_PAIR: OnceLock<minisign::KeyPair> = OnceLock::new();
    KEY_PAIR.get_or_init(|| minisign::KeyPair::generate_unencrypted_keypair().expect("test key pair"))
}

fn test_public_key() -> String {
    test_key_pair().pk.to_base64()
}

/// The path of the detached signature of `signed`.
fn signature_path(signed: &Path) -> PathBuf {
    PathBuf::from(format!("{}.minisig", signed.display()))
}

/// Sign `signed` as the graphrt-plugins release does: `<file>.minisig` next to it.
fn sign_with(key_pair: &minisign::KeyPair, signed: &Path) {
    let bytes = std::fs::read(signed).expect("read the file to sign");
    let comment = "graphrt-plugins index.json plugins=0 generated_at=test";
    let signature = minisign::sign(Some(&key_pair.pk), &key_pair.sk, std::io::Cursor::new(bytes), Some(comment), None)
        .expect("sign");
    std::fs::write(signature_path(signed), signature.into_string()).expect("write signature");
}

fn sign(signed: &Path) {
    sign_with(test_key_pair(), signed);
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// One tar entry of a fixture tarball.
struct TarEntry {
    path: String,
    type_flag: u8,
    content: Vec<u8>,
    link_target: String,
    mode: u32,
}

impl TarEntry {
    fn directory(path: &str) -> Self {
        Self { path: path.into(), type_flag: b'5', content: Vec::new(), link_target: String::new(), mode: 0o755 }
    }
    fn file(path: &str, content: &[u8]) -> Self {
        Self { path: path.into(), type_flag: b'0', content: content.to_vec(), link_target: String::new(), mode: 0o644 }
    }
    fn executable(path: &str, content: &[u8]) -> Self {
        Self { mode: 0o755, ..Self::file(path, content) }
    }
    fn link(path: &str, type_flag: u8, target: &str) -> Self {
        Self { path: path.into(), type_flag, content: Vec::new(), link_target: target.into(), mode: 0o777 }
    }
}

fn write_octal(field: &mut [u8], value: u64) {
    let text = format!("{value:0width$o}\0", width = field.len() - 1);
    field.copy_from_slice(text.as_bytes());
}

fn tar_header(entry: &TarEntry) -> [u8; 512] {
    let mut header = [0u8; 512];
    header[..entry.path.len()].copy_from_slice(entry.path.as_bytes());
    write_octal(&mut header[100..108], u64::from(entry.mode));
    write_octal(&mut header[108..116], 0);
    write_octal(&mut header[116..124], 0);
    write_octal(&mut header[124..136], entry.content.len() as u64);
    write_octal(&mut header[136..148], 0);
    header[156] = entry.type_flag;
    header[157..157 + entry.link_target.len()].copy_from_slice(entry.link_target.as_bytes());
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    header[148..156].copy_from_slice(b"        ");
    let checksum: u32 = header.iter().map(|&byte| u32::from(byte)).sum();
    header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
    header
}

fn build_tar_gz(entries: &[TarEntry]) -> Vec<u8> {
    let mut tar_bytes = Vec::new();
    for entry in entries {
        tar_bytes.extend_from_slice(&tar_header(entry));
        tar_bytes.extend_from_slice(&entry.content);
        tar_bytes.resize(tar_bytes.len().div_ceil(512) * 512, 0);
    }
    tar_bytes.extend_from_slice(&[0u8; 1024]);
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&tar_bytes).expect("gzip");
    encoder.finish().expect("gzip finish")
}

const RUN_SCRIPT: &[u8] = b"#!/bin/sh\necho hi\n";

/// The entries of a valid release tarball for plugin `name` at `version`, laid
/// out like graphrt-plugins' pack-plugins.mjs: one top-level `<name>/` directory.
fn plugin_entries(name: &str, version: &str) -> Vec<TarEntry> {
    let manifest = serde_json::json!({
        "name": name,
        "version": version,
        "description": format!("The {name} plugin"),
        "runtime": "process",
        "command": "sh",
        "args": ["run.sh"],
        "checksums": { "run.sh": sha256_hex(RUN_SCRIPT) },
        "nodes": [{ "kind": format!("{name}.echo"), "title": "Echo" }]
    });
    vec![
        TarEntry::directory(name),
        TarEntry::file(&format!("{name}/graphrt-plugin.json"), manifest.to_string().as_bytes()),
        TarEntry::executable(&format!("{name}/run.sh"), RUN_SCRIPT),
        TarEntry::directory(&format!("{name}/lib")),
        TarEntry::file(&format!("{name}/lib/notes.txt"), b"nested file"),
    ]
}

/// A catalog entry under construction: the tarball bytes plus the labels the index carries.
struct FixturePlugin {
    name: &'static str,
    version: &'static str,
    tarball: Vec<u8>,
    /// `None` writes `null`, like a catalog published before the labels existed.
    status: Option<&'static str>,
    tos_risk: Option<&'static str>,
    /// Overrides the tarball's real sha256 in the index.
    sha256_override: Option<String>,
    /// Overrides the tarball's real size in the index.
    size_override: Option<u64>,
    /// Overrides the `file://` tarball URL in the index.
    url_override: Option<String>,
}

impl FixturePlugin {
    fn valid(name: &'static str, version: &'static str) -> Self {
        Self::with_entries(name, version, &plugin_entries(name, version))
    }

    fn with_entries(name: &'static str, version: &'static str, entries: &[TarEntry]) -> Self {
        Self {
            name,
            version,
            tarball: build_tar_gz(entries),
            status: Some("beta"),
            tos_risk: Some("low"),
            sha256_override: None,
            size_override: None,
            url_override: None,
        }
    }
}

/// Write the tarballs and `index.json` into `catalog_dir`; returns the index path.
fn write_catalog(catalog_dir: &Path, plugins: &[FixturePlugin]) -> PathBuf {
    std::fs::create_dir_all(catalog_dir).expect("catalog dir");
    let entries: Vec<serde_json::Value> = plugins
        .iter()
        .map(|plugin| {
            let tarball_path = catalog_dir.join(format!("{}-{}.tar.gz", plugin.name, plugin.version));
            std::fs::write(&tarball_path, &plugin.tarball).expect("write tarball");
            serde_json::json!({
                "name": plugin.name,
                "version": plugin.version,
                "description": format!("The {} plugin\nsecond line is not shown", plugin.name),
                "tarball_url": plugin.url_override.clone().unwrap_or_else(|| format!("file://{}", tarball_path.display())),
                "sha256": plugin.sha256_override.clone().unwrap_or_else(|| sha256_hex(&plugin.tarball)),
                "size_bytes": plugin.size_override.unwrap_or(plugin.tarball.len() as u64),
                "permissions": { "network": false, "secrets": false, "env": [] },
                "needs_browser": false,
                "node": ">=20",
                "warning": null,
                "status": plugin.status,
                "status_reason": format!("{} fixture", plugin.status.unwrap_or("unlabelled")),
                "tos_risk": plugin.tos_risk,
                "operations_count": 1,
                "nodes": [],
                "tools": [],
                "readme_url": "https://example.invalid/README.md"
            })
        })
        .collect();
    let index = serde_json::json!({ "schema": 1, "generated_at": "2026-11-19T00:00:00Z", "plugins": entries });
    let index_path = catalog_dir.join("index.json");
    std::fs::write(&index_path, serde_json::to_string_pretty(&index).expect("index json")).expect("write index");
    sign(&index_path);
    index_path
}

/// `plugin install <spec> --index <index> --dir <home>/installed [extra...]`.
fn install(home: &Path, spec: &str, index: &Path, extra: &[&str]) -> Output {
    let install_dir = home.join("installed");
    let mut args = vec!["plugin", "install", spec, "--index", index.to_str().unwrap()];
    args.extend(["--dir", install_dir.to_str().unwrap()]);
    args.extend(extra);
    graphrt(home, &args)
}

fn installed_files(home: &Path) -> usize {
    match std::fs::read_dir(home.join("installed")) {
        Ok(entries) => entries.count(),
        Err(_) => 0,
    }
}

#[test]
fn installs_the_latest_version_by_name() {
    let home = scratch("latest");
    let index = write_catalog(
        &home.join("catalog"),
        &[
            FixturePlugin::valid("demo", "0.2.0"),
            FixturePlugin::valid("demo", "0.10.0"),
            FixturePlugin::valid("other", "1.0.0"),
        ],
    );
    let output = install(&home, "demo", &index, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("demo 0.10.0: status beta, ToS risk low"), "{text}");
    assert!(text.contains("installed demo 0.10.0"), "{text}");
    assert!(text.contains("demo.echo"), "{text}");
    let plugin_dir = home.join("installed/demo");
    assert!(plugin_dir.join("graphrt-plugin.json").is_file());
    assert_eq!(std::fs::read_to_string(plugin_dir.join("lib/notes.txt")).unwrap(), "nested file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(plugin_dir.join("run.sh")).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "run.sh keeps its executable bit: {mode:o}");
    }
    // Nothing else lands in the install directory (no staging leftovers).
    assert_eq!(installed_files(&home), 1);
}

#[test]
fn installs_an_exact_version_with_name_at_version() {
    let home = scratch("exact");
    let index = write_catalog(
        &home.join("catalog"),
        &[FixturePlugin::valid("demo", "0.1.0"), FixturePlugin::valid("demo", "0.2.0")],
    );
    let output = install(&home, "demo@0.1.0", &index, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("installed demo 0.1.0"), "{}", stdout(&output));
    let manifest = std::fs::read_to_string(home.join("installed/demo/graphrt-plugin.json")).unwrap();
    assert!(manifest.contains("\"0.1.0\""), "{manifest}");

    let output = install(&home, "demo@9.9.9", &index, &[]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("no version 9.9.9 of `demo`"), "{}", stderr(&output));
    assert!(stderr(&output).contains("0.1.0, 0.2.0"), "{}", stderr(&output));
}

#[test]
fn reads_the_index_location_from_the_environment() {
    let home = scratch("env-index");
    let index = write_catalog(&home.join("catalog"), &[FixturePlugin::valid("demo", "1.0.0")]);
    let install_dir = home.join("installed");
    let output = graphrt_with_env(
        &home,
        &["plugin", "install", "demo", "--dir", install_dir.to_str().unwrap()],
        &[("GRAPHRT_PLUGIN_INDEX", &format!("file://{}", index.display()))],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(install_dir.join("demo/run.sh").is_file());
}

#[test]
fn unknown_name_is_a_usage_error() {
    let home = scratch("unknown");
    let index = write_catalog(&home.join("catalog"), &[FixturePlugin::valid("demo", "1.0.0")]);
    let output = install(&home, "nosuch", &index, &[]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("`nosuch` is not in the plugin catalog"), "{}", stderr(&output));
    assert!(stderr(&output).contains("graphrt plugin search"), "{}", stderr(&output));
    assert_eq!(installed_files(&home), 0);
}

#[test]
fn checksum_mismatch_is_refused() {
    let home = scratch("checksum");
    let mut tampered = FixturePlugin::valid("demo", "1.0.0");
    tampered.sha256_override = Some("0".repeat(64));
    let index = write_catalog(&home.join("catalog"), &[tampered]);
    let output = install(&home, "demo", &index, &[]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("sha256 mismatch"), "{}", stderr(&output));
    assert_eq!(installed_files(&home), 0);
}

#[test]
fn path_traversal_tarball_is_refused() {
    let home = scratch("traversal");
    let mut entries = plugin_entries("demo", "1.0.0");
    entries.push(TarEntry::file("demo/../../escaped.txt", b"owned"));
    let index = write_catalog(&home.join("catalog"), &[FixturePlugin::with_entries("demo", "1.0.0", &entries)]);
    let output = install(&home, "demo", &index, &[]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("`..`"), "{}", stderr(&output));
    assert!(!home.join("escaped.txt").exists());
    assert!(!std::env::temp_dir().join("escaped.txt").exists());
    assert_eq!(installed_files(&home), 0);
}

#[test]
fn symlink_and_hardlink_entries_are_refused() {
    for (label, type_flag) in [("symlink", b'2'), ("hard link", b'1')] {
        let home = scratch(&format!("link-{}", type_flag as char));
        let mut entries = plugin_entries("demo", "1.0.0");
        entries.push(TarEntry::link("demo/passwd", type_flag, "/etc/passwd"));
        let index = write_catalog(&home.join("catalog"), &[FixturePlugin::with_entries("demo", "1.0.0", &entries)]);
        let output = install(&home, "demo", &index, &[]);
        assert_eq!(output.status.code(), Some(1), "{label}: {}", stderr(&output));
        assert!(stderr(&output).contains(label), "{label}: {}", stderr(&output));
        assert_eq!(installed_files(&home), 0, "{label}");
    }
}

#[test]
fn tarball_whose_manifest_disagrees_with_the_index_is_refused() {
    let home = scratch("mismatch");
    // The index says demo 1.0.0, the tarball's manifest says demo 2.0.0.
    let wrong_version = FixturePlugin::with_entries("demo", "1.0.0", &plugin_entries("demo", "2.0.0"));
    // The index says impostor, the tarball's `impostor/` directory holds plugin `other`.
    let mut renamed = plugin_entries("other", "1.0.0");
    for entry in &mut renamed {
        entry.path = entry.path.replacen("other", "impostor", 1);
    }
    let wrong_name = FixturePlugin::with_entries("impostor", "1.0.0", &renamed);
    let index = write_catalog(&home.join("catalog"), &[wrong_version, wrong_name]);

    let output = install(&home, "demo", &index, &[]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("the tarball holds `demo` 2.0.0 instead"), "{}", stderr(&output));
    let output = install(&home, "impostor", &index, &[]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("the tarball holds `other` 1.0.0 instead"), "{}", stderr(&output));
    assert_eq!(installed_files(&home), 0);
}

#[test]
fn tarball_larger_than_the_catalog_says_is_refused() {
    let home = scratch("oversized");
    let mut oversized = FixturePlugin::valid("demo", "1.0.0");
    oversized.size_override = Some(oversized.tarball.len() as u64 - 1);
    let index = write_catalog(&home.join("catalog"), &[oversized]);
    let output = install(&home, "demo", &index, &[]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("larger than"), "{}", stderr(&output));
    assert_eq!(installed_files(&home), 0);
}

#[test]
fn experimental_or_high_risk_plugins_need_yes() {
    let home = scratch("experimental");
    let mut experimental = FixturePlugin::valid("demo", "1.0.0");
    experimental.status = Some("experimental");
    let mut risky = FixturePlugin::valid("risky", "1.0.0");
    risky.tos_risk = Some("high");
    let index = write_catalog(&home.join("catalog"), &[experimental, risky]);

    for name in ["demo", "risky"] {
        let output = install(&home, name, &index, &[]);
        assert_eq!(output.status.code(), Some(2), "{name}: {}", stderr(&output));
        assert!(stderr(&output).contains("--yes"), "{name}: {}", stderr(&output));
        assert!(stderr(&output).contains("no interactive prompt"), "{name}: {}", stderr(&output));
    }
    assert!(stderr(&install(&home, "demo", &index, &[])).contains("experimental"));
    assert!(stderr(&install(&home, "risky", &index, &[])).contains("ToS risk high"));
    assert_eq!(installed_files(&home), 0);

    let output = install(&home, "demo", &index, &["--yes"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("demo 1.0.0: status experimental, ToS risk low"), "{}", stdout(&output));
    assert!(stdout(&output).contains("installed demo 1.0.0"), "{}", stdout(&output));
}

#[test]
fn unlabelled_or_unrecognised_labels_need_yes() {
    let home = scratch("unlabelled");
    let mut unlabelled = FixturePlugin::valid("bare", "1.0.0");
    unlabelled.status = None;
    unlabelled.tos_risk = None;
    let mut no_risk_label = FixturePlugin::valid("halfway", "1.0.0");
    no_risk_label.tos_risk = None;
    let mut capitalised = FixturePlugin::valid("shouty", "1.0.0");
    capitalised.status = Some("Experimental");
    let mut padded = FixturePlugin::valid("padded", "1.0.0");
    padded.tos_risk = Some(" HIGH");
    let mut deprecated = FixturePlugin::valid("old", "1.0.0");
    deprecated.status = Some("deprecated");
    let mut fine = FixturePlugin::valid("fine", "1.0.0");
    fine.status = Some(" Stable ");
    fine.tos_risk = Some("Medium");
    let index =
        write_catalog(&home.join("catalog"), &[unlabelled, no_risk_label, capitalised, padded, deprecated, fine]);

    let expectations = [
        ("bare", "this catalog entry has no status/ToS label"),
        ("halfway", "this catalog entry has no ToS risk label"),
        ("shouty", "status experimental"),
        ("padded", "ToS risk high"),
        ("old", "unrecognised status `deprecated`"),
    ];
    for (name, reason) in expectations {
        let output = install(&home, name, &index, &[]);
        assert_eq!(output.status.code(), Some(2), "{name}: {}", stderr(&output));
        assert!(stderr(&output).contains(reason), "{name}: {}", stderr(&output));
        assert!(stderr(&output).contains("re-run with `--yes`"), "{name}: {}", stderr(&output));
    }
    assert_eq!(installed_files(&home), 0);

    let output = install(&home, "fine", &index, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("fine 1.0.0: status stable, ToS risk medium"), "{}", stdout(&output));
    let output = install(&home, "bare", &index, &["--yes"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("bare 1.0.0: status unknown, ToS risk unknown"), "{}", stdout(&output));
}

/// Rewrite a written `index.json` in place, without signing it again: what
/// somebody who can replace the file, but has no signing key, can do.
fn tamper_with_index(index_path: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    let mut index: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(index_path).unwrap()).unwrap();
    edit(&mut index);
    std::fs::write(index_path, index.to_string()).unwrap();
}

/// Rewrite a written `index.json` in place and sign it again, as its publisher would.
fn republish_index(index_path: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    tamper_with_index(index_path, edit);
    sign(index_path);
}

/// Republish with `edit` applied to every entry.
fn edit_index(index_path: &Path, edit: impl Fn(&mut serde_json::Value)) {
    republish_index(index_path, |index| {
        for entry in index["plugins"].as_array_mut().unwrap() {
            edit(entry);
        }
    });
}

/// OSC 52 would set the terminal clipboard; CSI 2J clears the screen; U+009B is a C1 CSI.
const TERMINAL_ESCAPES: &str = "\u{1b}]52;c;SGVsbG8=\u{7}\u{1b}[2J\u{9b}31m\u{7f}";

fn assert_no_control_characters(label: &str, text: &str) {
    let control: Vec<char> = text.chars().filter(|c| c.is_control() && *c != '\n').collect();
    assert!(control.is_empty(), "{label} prints control characters {control:?}: {text:?}");
}

#[test]
fn index_strings_are_printed_without_control_characters() {
    let home = scratch("escapes");
    let mut hostile = FixturePlugin::valid("demo", "1.0.0");
    hostile.status = Some("experimental");
    let index = write_catalog(&home.join("catalog"), &[hostile, FixturePlugin::valid("other", "1.0.0")]);
    edit_index(&index, |entry| {
        entry["description"] = format!("Nice {TERMINAL_ESCAPES} plugin").into();
        entry["status_reason"] = format!("reason {TERMINAL_ESCAPES}").into();
        entry["warning"] = format!("warning {TERMINAL_ESCAPES}").into();
        if entry["name"] == "other" {
            entry["status"] = format!("beta{TERMINAL_ESCAPES}").into();
            entry["tos_risk"] = format!("low{TERMINAL_ESCAPES}").into();
        }
    });
    republish_index(&index, |index_value| {
        let mut invalid_name = index_value["plugins"][0].clone();
        invalid_name["name"] = format!("evil{TERMINAL_ESCAPES}").into();
        let mut invalid_version = index_value["plugins"][0].clone();
        invalid_version["version"] = format!("9.9.9{TERMINAL_ESCAPES}").into();
        index_value["plugins"].as_array_mut().unwrap().extend([invalid_name, invalid_version]);
    });

    let search = graphrt(&home, &["plugin", "search", "--index", index.to_str().unwrap()]);
    assert!(search.status.success(), "{}", stderr(&search));
    assert_no_control_characters("search", &stdout(&search));
    assert!(!stdout(&search).contains("evil"), "entries with invalid names are skipped: {}", stdout(&search));
    assert!(!stdout(&search).contains("9.9.9"), "entries with invalid versions are skipped: {}", stdout(&search));
    assert!(stdout(&search).contains("Nice ?]52;c;SGVsbG8=??[2J?31m? plugin"), "{}", stdout(&search));

    for name in ["demo", "other", "demo@9.9.9"] {
        let output = install(&home, name, &index, &[]);
        assert_eq!(output.status.code(), Some(2), "{name}: {}", stderr(&output));
        assert_no_control_characters(&format!("{name} stdout"), &stdout(&output));
        assert_no_control_characters(&format!("{name} stderr"), &stderr(&output));
    }
    let other = install(&home, "other", &index, &[]);
    assert!(stderr(&other).contains("unrecognised status `beta?]52;c;"), "{}", stderr(&other));
    assert!(stdout(&install(&home, "demo", &index, &[])).contains("reason ?]52;c;"));
}

#[test]
fn non_https_tarball_urls_are_refused() {
    let home = scratch("plain-http");
    let mut plain_http = FixturePlugin::valid("demo", "1.0.0");
    plain_http.url_override = Some("http://127.0.0.1:9/demo-1.0.0.tar.gz".into());
    let index = write_catalog(&home.join("catalog"), &[plain_http]);
    let output = install(&home, "demo", &index, &[]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("https"), "{}", stderr(&output));
}

#[test]
fn catalog_flags_only_apply_to_catalog_names() {
    let home = scratch("flags");
    let index = write_catalog(&home.join("catalog"), &[FixturePlugin::valid("demo", "1.0.0")]);
    let output = install(&home, "demo", &index, &["--rev", "v1"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let local_dir = home.join("catalog");
    let output = install(&home, local_dir.to_str().unwrap(), &index, &[]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("`--index` only applies"), "{}", stderr(&output));
    let install_dir = home.join("installed");
    for (flag, value) in [("--index-key", Some("some-key")), ("--allow-unsigned-index", None)] {
        let mut args = vec!["plugin", "install", local_dir.to_str().unwrap(), "--dir", install_dir.to_str().unwrap()];
        args.push(flag);
        args.extend(value);
        let output = graphrt(&home, &args);
        assert_eq!(output.status.code(), Some(2), "{flag}: {}", stderr(&output));
        assert!(stderr(&output).contains(&format!("`{flag}` only applies")), "{flag}: {}", stderr(&output));
    }
}

#[test]
fn yes_is_rejected_for_directory_and_git_sources() {
    let home = scratch("yes-flag");
    let install_dir = home.join("installed");
    let example = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/examples/python-sentiment");
    let git_url = "https://example.invalid/acme/graphrt-acme.git";
    for source in [example.to_str().unwrap(), git_url] {
        let output = graphrt(&home, &["plugin", "install", source, "--yes", "--dir", install_dir.to_str().unwrap()]);
        assert_eq!(output.status.code(), Some(2), "{source}: {}", stderr(&output));
        assert!(
            stderr(&output).contains("`--yes` only applies to catalog plugin names"),
            "{source}: {}",
            stderr(&output)
        );
    }
    assert_eq!(installed_files(&home), 0);
    // Without `--yes` the same directory installs.
    let output =
        graphrt(&home, &["plugin", "install", example.to_str().unwrap(), "--dir", install_dir.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
}

#[test]
fn a_local_directory_named_like_a_plugin_wins_over_the_catalog() {
    let home = scratch("name-clash");
    let index = write_catalog(&home.join("catalog"), &[FixturePlugin::valid("demo", "1.0.0")]);
    let work_dir = home.join("work");
    std::fs::create_dir_all(work_dir.join("demo")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_graphrt"))
        .args(["plugin", "install", "demo", "--index", index.to_str().unwrap()])
        .current_dir(&work_dir)
        .env("HOME", &home)
        .env_remove("GRAPHRT_PLUGIN_PATH")
        .output()
        .expect("spawn graphrt");
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("`demo` is a local directory"), "{}", stderr(&output));
}

#[test]
fn search_lists_catalog_entries() {
    let home = scratch("search");
    let mut experimental = FixturePlugin::valid("weather", "0.3.0");
    experimental.status = Some("experimental");
    let index = write_catalog(
        &home.join("catalog"),
        &[FixturePlugin::valid("demo", "0.1.0"), FixturePlugin::valid("demo", "0.2.0"), experimental],
    );
    let output = graphrt(&home, &["plugin", "search", "--index", index.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    assert!(lines[0].starts_with("NAME"), "{text}");
    assert!(lines[1].starts_with("demo") && lines[1].contains("0.2.0") && lines[1].contains("beta"), "{text}");
    assert!(lines[1].contains("The demo plugin") && !text.contains("second line"), "{text}");
    assert!(lines[2].starts_with("weather") && lines[2].contains("experimental"), "{text}");

    let output = graphrt(&home, &["plugin", "search", "WEATH", "--index", index.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).lines().count(), 2, "{}", stdout(&output));

    let output = graphrt(&home, &["plugin", "search", "zzz", "--index", index.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("no catalog plugins match `zzz`"), "{}", stdout(&output));
}

#[test]
fn unreadable_or_unsupported_indexes_fail_clearly() {
    let home = scratch("bad-index");
    let missing = home.join("missing.json");
    let output = graphrt(&home, &["plugin", "search", "--index", missing.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("missing.json"), "{}", stderr(&output));

    // A signed index of a newer format (`index-v2.json` served as `index.json`) is not read as schema 1.
    let future = home.join("future.json");
    std::fs::write(&future, r#"{"schema": 2, "plugins": []}"#).unwrap();
    sign(&future);
    let output = graphrt(&home, &["plugin", "search", "--index", future.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("schema 2"), "{}", stderr(&output));

    let output = graphrt(&home, &["plugin", "search", "--index", "http://127.0.0.1:9/index.json"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("https"), "{}", stderr(&output));
}

/// The entries of [`plugin_entries`] with `edit` applied to the manifest.
fn entries_with_manifest(name: &str, version: &str, edit: impl Fn(&mut serde_json::Value)) -> Vec<TarEntry> {
    let mut entries = plugin_entries(name, version);
    let manifest_path = format!("{name}/graphrt-plugin.json");
    for entry in entries.iter_mut().filter(|entry| entry.path == manifest_path) {
        let mut manifest: serde_json::Value = serde_json::from_slice(&entry.content).unwrap();
        edit(&mut manifest);
        entry.content = manifest.to_string().into_bytes();
    }
    entries
}

#[test]
fn tarball_strings_are_printed_without_control_characters() {
    let home = scratch("tarball-escapes");
    // A refused entry path is quoted in the error.
    let mut hostile_path = plugin_entries("demo", "1.0.0");
    hostile_path.push(TarEntry::file("demo/\u{1b}]52;c;SGVsbG8=\u{7}/../x", b"x"));
    // Manifest fields end up in validation errors and in the "installed" line.
    let hostile_kind = entries_with_manifest("kinds", "1.0.0", |manifest| {
        manifest["nodes"][0]["kind"] = format!("kinds.echo{TERMINAL_ESCAPES}").into();
    });
    let hostile_field = entries_with_manifest("fields", "1.0.0", |manifest| {
        manifest[format!("x{TERMINAL_ESCAPES}")] = true.into();
    });
    let index = write_catalog(
        &home.join("catalog"),
        &[
            FixturePlugin::with_entries("demo", "1.0.0", &hostile_path),
            FixturePlugin::with_entries("kinds", "1.0.0", &hostile_kind),
            FixturePlugin::with_entries("fields", "1.0.0", &hostile_field),
        ],
    );
    for name in ["demo", "kinds", "fields"] {
        let output = install(&home, name, &index, &[]);
        assert_eq!(output.status.code(), Some(1), "{name}: {}", stderr(&output));
        assert_no_control_characters(&format!("{name} stdout"), &stdout(&output));
        assert_no_control_characters(&format!("{name} stderr"), &stderr(&output));
    }
    assert_eq!(installed_files(&home), 0);
}

/// `plugin search --index <index> [extra...]`.
fn search(home: &Path, index: &Path, extra: &[&str], env: &[(&str, &str)]) -> Output {
    let mut args = vec!["plugin", "search", "--index", index.to_str().unwrap()];
    args.extend(extra);
    graphrt_with_env(home, &args, env)
}

/// `plugin install demo` with `env`, into `<home>/installed`.
fn install_with_env(home: &Path, index: &Path, extra: &[&str], env: &[(&str, &str)]) -> Output {
    let install_dir = home.join("installed");
    let mut args = vec!["plugin", "install", "demo", "--index", index.to_str().unwrap()];
    args.extend(["--dir", install_dir.to_str().unwrap()]);
    args.extend(extra);
    graphrt_with_env(home, &args, env)
}

/// The built-in graphrt-plugins key is the trusted one (no key named).
const BUILT_IN_KEY: &[(&str, &str)] = &[(INDEX_KEY_ENV, "")];

/// Both commands refuse the index with `message`, and nothing is installed.
fn assert_index_refused(home: &Path, index: &Path, env: &[(&str, &str)], message: &str) {
    let output = install_with_env(home, index, &[], env);
    assert_eq!(output.status.code(), Some(1), "install: {}", stderr(&output));
    assert!(stderr(&output).contains(message), "install: {}", stderr(&output));
    assert!(stderr(&output).contains("The index was not used"), "install: {}", stderr(&output));
    assert!(!stdout(&output).contains("demo"), "nothing of the index is printed: {}", stdout(&output));
    assert_eq!(installed_files(home), 0);
    let output = search(home, index, &[], env);
    assert_eq!(output.status.code(), Some(1), "search: {}", stderr(&output));
    assert!(stderr(&output).contains(message), "search: {}", stderr(&output));
    assert_eq!(stdout(&output), "", "nothing of the index is listed");
}

#[test]
fn an_index_without_a_signature_is_refused() {
    let home = scratch("unsigned");
    let index = write_catalog(&home.join("catalog"), &[FixturePlugin::valid("demo", "1.0.0")]);
    std::fs::remove_file(signature_path(&index)).unwrap();
    assert_index_refused(&home, &index, &[], "the plugin catalog signature is missing");
    assert_index_refused(&home, &index, BUILT_IN_KEY, "the plugin catalog signature is missing");
    // Not a signature at all, such as the HTML of an error page.
    std::fs::write(signature_path(&index), "<html>not found</html>").unwrap();
    assert_index_refused(&home, &index, &[], "the signature cannot be read");
    std::fs::write(signature_path(&index), [0xff, 0xfe, 0x00]).unwrap();
    assert_index_refused(&home, &index, &[], "is not text");
    std::fs::write(signature_path(&index), "x".repeat(8 * 1024)).unwrap();
    assert_index_refused(&home, &index, &[], "4096-byte limit");
}

/// The attack this closes: whoever serves the index signs it with a key of their own.
#[test]
fn an_index_signed_with_another_key_is_refused() {
    let home = scratch("other-key");
    let index = write_catalog(&home.join("catalog"), &[FixturePlugin::valid("demo", "1.0.0")]);
    // Signed with the test key, but only the built-in graphrt-plugins key is trusted.
    assert_index_refused(&home, &index, BUILT_IN_KEY, "it was not made with a trusted catalog key");
    // Signed with a third key, while the test key is the trusted one.
    let attacker = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    sign_with(&attacker, &index);
    assert_index_refused(&home, &index, &[], "it was not made with a trusted catalog key");
    let output = install_with_env(&home, &index, &["--index-key", &test_public_key()], BUILT_IN_KEY);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("must be signed with the key from `--index-key`"), "{}", stderr(&output));
    // The same index is fine once its real signer is the trusted key.
    let output = install_with_env(&home, &index, &["--index-key", &attacker.pk.to_base64()], BUILT_IN_KEY);
    assert!(output.status.success(), "{}", stderr(&output));
}

#[test]
fn an_index_changed_after_signing_is_refused() {
    let home = scratch("tampered");
    let catalog_dir = home.join("catalog");
    let index = write_catalog(&catalog_dir, &[FixturePlugin::valid("demo", "1.0.0")]);
    // Point the entry at another tarball, with that tarball's own sha256.
    let evil_tarball = build_tar_gz(&entries_with_manifest("demo", "1.0.0", |manifest| {
        manifest["description"] = "not the published plugin".into();
    }));
    let evil_path = catalog_dir.join("evil.tar.gz");
    std::fs::write(&evil_path, &evil_tarball).unwrap();
    tamper_with_index(&index, |index_value| {
        let entry = &mut index_value["plugins"][0];
        entry["tarball_url"] = format!("file://{}", evil_path.display()).into();
        entry["sha256"] = sha256_hex(&evil_tarball).into();
        entry["size_bytes"] = evil_tarball.len().into();
    });
    assert_index_refused(&home, &index, &[], "the signature does not match the index");

    // The signature is checked before the index is parsed: bytes that are not
    // even JSON are refused for their signature.
    std::fs::write(&index, "{ not json").unwrap();
    assert_index_refused(&home, &index, &[], "the signature does not match the index");

    // One byte appended to the published index.
    let index = write_catalog(&catalog_dir, &[FixturePlugin::valid("demo", "1.0.0")]);
    let mut longer = std::fs::read(&index).unwrap();
    longer.push(b'\n');
    std::fs::write(&index, longer).unwrap();
    assert_index_refused(&home, &index, &[], "the signature does not match the index");
}

/// The signature of one index does not make another index trusted.
#[test]
fn a_signature_of_another_index_is_refused() {
    let home = scratch("swapped");
    let old = write_catalog(&home.join("old"), &[FixturePlugin::valid("demo", "1.0.0")]);
    let new = write_catalog(&home.join("new"), &[FixturePlugin::valid("demo", "2.0.0")]);
    std::fs::copy(signature_path(&old), signature_path(&new)).unwrap();
    assert_index_refused(&home, &new, &[], "the signature does not match the index");
}

#[test]
fn a_tarball_replaced_under_a_verified_index_is_refused() {
    let home = scratch("swapped-tarball");
    let catalog_dir = home.join("catalog");
    let index = write_catalog(&catalog_dir, &[FixturePlugin::valid("demo", "1.0.0")]);
    // Same size as the published tarball, so only the sha256 can tell.
    let tarball_path = catalog_dir.join("demo-1.0.0.tar.gz");
    let mut replaced = std::fs::read(&tarball_path).unwrap();
    let middle = replaced.len() / 2;
    replaced[middle] ^= 0x01;
    std::fs::write(&tarball_path, replaced).unwrap();
    let output = install(&home, "demo", &index, &[]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("sha256 mismatch"), "{}", stderr(&output));
    assert_eq!(installed_files(&home), 0);
}

fn set_generated_at(index: &Path, generated_at: &str) {
    republish_index(index, |index_value| index_value["generated_at"] = generated_at.into());
}

/// Put the index and signature of `from` where `to` is served: what somebody
/// who can replace the published files, but cannot sign, can replay.
fn serve_instead(from: &Path, to: &Path) {
    std::fs::copy(from, to).unwrap();
    std::fs::copy(signature_path(from), signature_path(to)).unwrap();
}

/// A validly signed but older index (with a withdrawn version) is not accepted
/// once a newer one from the same place and key was used.
#[test]
fn an_older_signed_index_is_refused_as_a_rollback() {
    let home = scratch("rollback");
    let index = write_catalog(&home.join("catalog"), &[FixturePlugin::valid("demo", "2.0.0")]);
    set_generated_at(&index, "2026-11-20T10:00:00Z");
    let output = search(&home, &index, &[], &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    let state = std::fs::read_to_string(home.join(".graphrt/plugin-catalog-state.json")).unwrap();
    assert!(state.contains("2026-11-20T10:00:00Z") && state.contains(&test_public_key()), "{state}");

    // The index published a day earlier, with its own valid signature, served in its place.
    let current = home.join("current.json");
    serve_instead(&index, &current);
    let old_index = write_catalog(&home.join("old-catalog"), &[FixturePlugin::valid("demo", "1.0.0")]);
    set_generated_at(&old_index, "2026-11-19T10:00:00Z");
    serve_instead(&old_index, &index);
    for output in [install_with_env(&home, &index, &[], &[]), search(&home, &index, &[], &[])] {
        assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
        assert!(stderr(&output).contains("refusing a possible rollback"), "{}", stderr(&output));
        assert!(stderr(&output).contains("2026-11-20T10:00:00Z"), "{}", stderr(&output));
    }
    assert_eq!(installed_files(&home), 0);
    republish_index(&index, |index_value| {
        index_value.as_object_mut().unwrap().remove("generated_at");
    });
    let output = install_with_env(&home, &index, &[], &[]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("no usable `generated_at`"), "{}", stderr(&output));

    // An unsigned local index is not tracked.
    let output = search(&home, &index, &["--allow-unsigned-index"], BUILT_IN_KEY);
    assert!(output.status.success(), "{}", stderr(&output));
    // Another key has its own history, and so has another place (the old catalog where it was written).
    let other = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    sign_with(&other, &index);
    let output = search(&home, &index, &["--index-key", &other.pk.to_base64()], BUILT_IN_KEY);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(search(&home, &old_index, &[], &[]).status.success());

    // The index that was used before comes back, then a newer one: both are fine.
    serve_instead(&current, &index);
    let output = install_with_env(&home, &index, &[], &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("installed demo 2.0.0"), "{}", stdout(&output));
    set_generated_at(&index, "2026-11-21T10:00:00.5Z");
    assert!(search(&home, &index, &[], &[]).status.success());
}

#[test]
fn a_catalog_of_another_publisher_needs_its_key_named() {
    let home = scratch("index-key");
    let index = write_catalog(&home.join("catalog"), &[FixturePlugin::valid("demo", "1.0.0")]);
    let key = test_public_key();
    // The base64 key on the command line.
    let output = install_with_env(&home, &index, &["--index-key", &key], BUILT_IN_KEY);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("is checked against the key from `--index-key`"), "{}", stderr(&output));
    assert!(stderr(&output).contains(&key), "the key is shown: {}", stderr(&output));
    // A minisign .pub file.
    let pub_file = home.join("publisher.pub");
    std::fs::write(&pub_file, format!("untrusted comment: minisign public key\n{key}\n")).unwrap();
    let output = search(&home, &index, &["--index-key", pub_file.to_str().unwrap()], BUILT_IN_KEY);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("demo"), "{}", stdout(&output));
    // The environment variable, named in the note.
    let output = search(&home, &index, &[], &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("the key from $GRAPHRT_PLUGIN_INDEX_KEY"), "{}", stderr(&output));
    // Something that is not a key is a usage error, not a reason to skip the check.
    for bad_key in ["not-a-key", "AAAA"] {
        let output = search(&home, &index, &["--index-key", bad_key], BUILT_IN_KEY);
        assert_eq!(output.status.code(), Some(2), "{bad_key}: {}", stderr(&output));
        assert!(stderr(&output).contains("is not a minisign public key"), "{bad_key}: {}", stderr(&output));
    }
}

#[test]
fn an_unsigned_index_needs_the_explicit_flag_and_a_local_file() {
    let home = scratch("allow-unsigned");
    let index = write_catalog(&home.join("catalog"), &[FixturePlugin::valid("demo", "1.0.0")]);
    std::fs::remove_file(signature_path(&index)).unwrap();
    let output = install_with_env(&home, &index, &["--allow-unsigned-index"], BUILT_IN_KEY);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("without a signature check (--allow-unsigned-index)"), "{}", stderr(&output));
    assert!(home.join("installed/demo/run.sh").is_file());
    assert!(!home.join(".graphrt/plugin-catalog-state.json").exists(), "unsigned indexes are not tracked");

    // Never for a catalog on the network, the default one included.
    let remote: &[&[&str]] = &[
        &["plugin", "search", "--allow-unsigned-index", "--index", "https://catalog.invalid/index.json"],
        &["plugin", "search", "--allow-unsigned-index"],
        &["plugin", "install", "demo", "--allow-unsigned-index"],
    ];
    for args in remote {
        let output = graphrt_with_env(&home, args, BUILT_IN_KEY);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {}", stderr(&output));
        assert!(stderr(&output).contains("only applies to a local index file"), "{args:?}: {}", stderr(&output));
    }
    // Not together with a key, from the flag or from the environment.
    let output = search(&home, &index, &["--allow-unsigned-index", "--index-key", &test_public_key()], BUILT_IN_KEY);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let output = search(&home, &index, &["--allow-unsigned-index"], &[]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("cannot be combined with $GRAPHRT_PLUGIN_INDEX_KEY"), "{}", stderr(&output));
}

/// Installing from a directory never reads a catalog, so it needs no signature.
#[test]
fn local_directory_installs_need_no_signature() {
    let home = scratch("local-dir");
    let install_dir = home.join("installed");
    let example = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/examples/python-sentiment");
    let args = ["plugin", "install", example.to_str().unwrap(), "--dir", install_dir.to_str().unwrap()];
    let output = graphrt_with_env(&home, &args, BUILT_IN_KEY);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(installed_files(&home), 1);
}
