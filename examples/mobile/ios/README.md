# graphrt iOS sample

A minimal SwiftUI app that embeds graphrt as a static XCFramework through the
UniFFI Swift binding and runs the offline
[`examples/hello.yaml`](../../hello.yaml) workflow (start → template → end)
on the device. No network, no API key.

- **Run** starts a run and waits for the final result (`RunHandle.waitJson`).
- **Stream events** prints every engine event as the run produces it
  (`EventListener.onEvent`, then `onFinish`).

`GraphrtSample.xcodeproj/project.pbxproj` is a small hand-written project with
no generator. It references repository files in place:
`bindings/swift/graphrt_ffi.swift` (compiled into the app),
`examples/hello.yaml` (bundled as a resource), and
`build/GraphrtFFI.xcframework` (built by the script below, not checked in).

## Prerequisites

- macOS with Xcode 16+ (tested with Xcode 26.6, iOS 26.5 SDK). The iOS SDKs
  come with Xcode; the Command Line Tools alone are not enough.
- Rust (stable, 1.86+). The script adds `aarch64-apple-ios` and
  `aarch64-apple-ios-sim` with rustup if they are missing.

## Build

From the repository root:

```sh
# 1. Build GraphrtFFI.xcframework (device arm64 + simulator arm64)
examples/mobile/ios/scripts/build-xcframework.sh
# -> examples/mobile/ios/build/GraphrtFFI.xcframework

# 2. Build the app for the simulator
cd examples/mobile/ios
xcodebuild -project GraphrtSample.xcodeproj -scheme GraphrtSample \
  -sdk iphonesimulator -configuration Debug -derivedDataPath build/DerivedData \
  CODE_SIGNING_ALLOWED=NO build
```

Or open `GraphrtSample.xcodeproj` in Xcode and run it. For a physical device,
choose your team under Signing & Capabilities (the bundle id is
`dev.graphrt.sample`; change it if Xcode asks).

The script builds with `--no-default-features` and the `min-size` profile,
which leaves out the JavaScript code node, MCP, document extraction and RAG.
The hello workflow needs none of them. To change this, set `GRAPHRT_FEATURES`
(for example `"--features mobile-js"`), `GRAPHRT_TARGETS` (add
`x86_64-apple-ios` for Intel simulators and drop the `EXCLUDED_ARCHS` setting
from the project), or `GRAPHRT_PROFILE`. If the shell's active developer
directory is the Command Line Tools, the script switches `DEVELOPER_DIR` to
`/Applications/Xcode.app`.

Sizes measured with `--no-default-features`, `min-size` and Rust 1.95:
`libgraphrt_ffi.a` is about 103 MB per slice, because a static archive keeps
every object and its symbols, and the XCFramework is 197 MB on disk. The
linker keeps only what the app uses, so the Release device app binary
(arm64, unsigned) is **13.7 MB**.

## Run on a simulator

```sh
xcrun simctl boot "iPhone 17"            # any iOS 16+ simulator
xcrun simctl install booted build/DerivedData/Build/Products/Debug-iphonesimulator/GraphrtSample.app
xcrun simctl launch --console-pty booted dev.graphrt.sample -name Grace -autorun YES
xcrun simctl io booted screenshot graphrt-sample.png
```

The launch arguments are optional and become `UserDefaults` values:
`-name Grace` fills in the name, and `-autorun YES` presses both buttons once
the workflow has loaded.

## Expected output

On screen, under the buttons:

```text
graphrt 0.3.0 loaded workflow 'hello'
Result
status: succeeded
greeting: Hello, Grace.
{"elapsed_ms":4,"outputs":{"greeting":"Hello, Grace."},...,"status":"succeeded","answer":""}
Events
event: run_started
event: node_started [start]
event: node_succeeded [start]
event: node_started [greet]
event: node_succeeded [greet]
event: node_started [end]
event: node_succeeded [end]
event: run_succeeded
finish: {"elapsed_ms":4,"outputs":{"greeting":"Hello, Grace."},...}
```

The same lines are printed to the console with a `[GraphrtSample]` prefix.
With **excited** on, the greeting is `Hello, Grace!!!`.

## Threading

`Runtime(configJson:)` and `waitJson` can block, so they run in detached tasks.
Listener callbacks arrive on graphrt's worker threads and hop to the main actor
before they change published state (`HelloWorkflowModel.swift`).
