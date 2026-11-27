#!/usr/bin/env bash
# Packages the Swift SDK for a GitHub release, after build-xcframework.sh:
#
#   dist/GraphrtFFI.xcframework.zip       the native library (iOS, simulator, macOS)
#   dist/graphrt-swift-<version>.zip      the Swift package: Package.swift pointing at
#                                         the release's GraphrtFFI.xcframework.zip by
#                                         URL and checksum, Sources/Graphrt, README
#
# An app adds the package by unzipping it next to the project (or into its own
# git repository) and adding it as a local package; Xcode downloads the
# XCFramework from the release and checks its checksum.
#
# Usage: sdk/swift/scripts/package-release.sh <tag> <owner/repo> [dist dir]
set -euo pipefail

if [[ $# -lt 2 ]]; then
  echo "usage: sdk/swift/scripts/package-release.sh <tag> <owner/repo> [dist dir]" >&2
  exit 2
fi
tag="$1"
repo="$2"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
package_dir="$(cd "$script_dir/.." && pwd)"
dist="$(mkdir -p "${3:-$package_dir/dist}" && cd "${3:-$package_dir/dist}" && pwd)"
version="${tag#v}"
xcframework="$package_dir/GraphrtFFI.xcframework"

if [[ ! -d "$xcframework" ]]; then
  echo "error: $xcframework not found; run sdk/swift/scripts/build-xcframework.sh first" >&2
  exit 1
fi

# `ditto` keeps the framework's symlinks and metadata as Xcode expects them.
(cd "$package_dir" && ditto -c -k --sequesterRsrc --keepParent GraphrtFFI.xcframework "$dist/GraphrtFFI.xcframework.zip")
checksum="$(swift package compute-checksum "$dist/GraphrtFFI.xcframework.zip")"

staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
root="$staging/graphrt-swift-$version"
mkdir -p "$root/Sources/Graphrt"
# Real files, not the repository's symlinks.
cp -L "$package_dir/Sources/Graphrt/"*.swift "$root/Sources/Graphrt/"
cp "$package_dir/README.md" "$root/README.md"
cat >"$root/Package.swift" <<SWIFT
// swift-tools-version:5.9
// Graphrt $version for Swift: the binding, Swift helpers and the native library
// of the GitHub release $tag (iOS 16+, macOS 13+).
import PackageDescription

let package = Package(
    name: "Graphrt",
    platforms: [.iOS(.v16), .macOS(.v13)],
    products: [
        .library(name: "Graphrt", targets: ["Graphrt"]),
    ],
    targets: [
        .binaryTarget(
            name: "GraphrtFFI",
            url: "https://github.com/$repo/releases/download/$tag/GraphrtFFI.xcframework.zip",
            checksum: "$checksum"
        ),
        .target(
            name: "Graphrt",
            dependencies: ["GraphrtFFI"],
            path: "Sources/Graphrt",
            linkerSettings: [
                .linkedFramework("Security"),
                .linkedFramework("SystemConfiguration"),
            ]
        ),
    ]
)
SWIFT
(cd "$staging" && zip -qr "$dist/graphrt-swift-$version.zip" "graphrt-swift-$version")
echo "GraphrtFFI.xcframework.zip checksum: $checksum"
ls -l "$dist"
