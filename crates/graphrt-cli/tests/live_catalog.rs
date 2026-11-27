//! Smoke test against the LIVE skill catalog (Graphrt/graphrt-plugins): the
//! default index URL over HTTPS, checked against the catalog key built into
//! the binary, exactly as a user's `graphrt skill install <name>` reads it.
//!
//! Read only: it downloads the signed index and one skill archive into a
//! scratch home and store, and changes nothing anywhere else.
//!
//! Opt-in twice, so a plain `cargo test` and pull requests never need the
//! network: the test is `#[ignore]`d, and it also skips unless
//! `GRAPHRT_LIVE_CATALOG=1` is set. `.github/workflows/live-catalog.yml` runs it
//! nightly and on manual dispatch.
//!
//! ```sh
//! GRAPHRT_LIVE_CATALOG=1 cargo test -p graphrt-cli --test live_catalog -- --ignored --nocapture
//! ```
#![cfg(all(feature = "skills", feature = "plugins"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing, clippy::panic, clippy::print_stdout)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value as Json;
use sha2::{Digest, Sha256};

mod common;
use common::catalog::key_pair;

/// Environment variable that switches the test on.
const LIVE_SWITCH: &str = "GRAPHRT_LIVE_CATALOG";

/// A scratch home and skill store, removed afterwards.
struct Scratch {
    root: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Scratch {
    fn new() -> Self {
        Self { root: graphrt_core::private_fs::create_fresh_temp_dir("graphrt-live-catalog").unwrap() }
    }

    fn store(&self) -> PathBuf {
        self.root.join("store")
    }

    /// `graphrt skill <args>` with the default catalog, and the
    /// catalog variables of the caller's environment removed so the test reads
    /// what a user reads. `key` replaces the built-in catalog key.
    fn skill(&self, args: &[&str], key: Option<&str>) -> Output {
        let home = self.root.join("home");
        let mut command = Command::new(env!("CARGO_BIN_EXE_graphrt"));
        command
            .arg("skill")
            .args(args)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("GRAPHRT_SKILLS_DIR", self.store())
            .env_remove("GRAPHRT_PLUGIN_INDEX")
            .env_remove("GRAPHRT_PLUGIN_INDEX_KEY");
        if let Some(key) = key {
            command.env("GRAPHRT_PLUGIN_INDEX_KEY", key);
        }
        command.output().expect("spawn graphrt")
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn succeeded(output: &Output, what: &str) -> String {
    assert!(
        output.status.success(),
        "{what} failed:\nstdout: {}\nstderr: {}",
        text(&output.stdout),
        text(&output.stderr)
    );
    text(&output.stdout)
}

fn sha256_hex(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    Sha256::digest(&bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
#[ignore = "reads the live catalog over the network: GRAPHRT_LIVE_CATALOG=1 ... -- --ignored"]
fn the_live_catalog_is_signed_and_installs_a_skill_whose_files_match_their_digests() {
    if std::env::var(LIVE_SWITCH).ok().as_deref() != Some("1") {
        println!("skipping live catalog test: set {LIVE_SWITCH}=1");
        return;
    }
    let scratch = Scratch::new();

    // The index verifies against the built-in key, or `search` would refuse it.
    let found = succeeded(&scratch.skill(&["search"], None), "skill search");
    let name = found
        .lines()
        .skip_while(|line| !line.starts_with("NAME"))
        .nth(1)
        .and_then(|line| line.split_whitespace().next())
        .unwrap_or_else(|| panic!("the live catalog lists no skill:\n{found}"))
        .to_owned();
    println!(
        "[live-catalog] index verified, {} skill(s), installing `{name}`",
        found.lines().count().saturating_sub(1)
    );

    // Signed by someone else: the same index is refused, so the check above was a real one.
    let other_key = key_pair().pk.to_base64();
    let refused = scratch.skill(&["search"], Some(&other_key));
    assert!(!refused.status.success(), "the live index verified against a key that did not sign it");

    // Installed from the catalog: the archive matched the digest the signed index names.
    succeeded(&scratch.skill(&["install", &name], None), "skill install");
    let listed: Json =
        serde_json::from_str(&succeeded(&scratch.skill(&["list", "--json"], None), "skill list")).unwrap();
    let skill = listed
        .as_array()
        .and_then(|skills| skills.iter().find(|skill| skill["name"] == name.as_str()))
        .unwrap_or_else(|| panic!("`{name}` is not in the store: {listed}"));
    let origin = &skill["origin"];
    assert_eq!(origin["kind"], "catalog", "{skill}");
    assert_eq!(origin["verified"], true, "{skill}");
    assert!(origin["detail"].as_str().is_some_and(|url| url.starts_with("https://")), "{skill}");
    let digest = skill["digest"].as_str().unwrap_or_default();
    assert!(digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()), "digest: {skill}");

    // Every installed file is the one the store recorded.
    let files = skill["files"].as_array().filter(|files| !files.is_empty()).unwrap_or_else(|| panic!("{skill}"));
    let dir = scratch.store().join("skills").join(&name);
    for file in files {
        let path = file["path"].as_str().unwrap();
        assert_eq!(sha256_hex(&dir.join(path)), file["sha256"].as_str().unwrap(), "{path} of `{name}`");
    }
    println!("[live-catalog] `{name}` {} installed, digest {digest}, {} file(s) match", origin["version"], files.len());
}
