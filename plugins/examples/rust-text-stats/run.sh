#!/bin/sh
# Starts the Rust SDK example built in the workspace target directory. Plugin
# manifests may not point `command` outside the plugin directory, so the
# manifest runs this script, which execs the binary.
# Build it first: cargo build --release -p graphrt-plugin-sdk --example text_stats
exec "$(dirname "$0")/../../../target/release/examples/text_stats" "$@"
