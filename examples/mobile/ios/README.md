# graphrt iOS sample

A minimal SwiftUI app that embeds Graphrt as a static XCFramework through the
Swift binding and the Swift package's helpers, and runs two offline workflows
on the device. No network, no API key.

- **Run** starts the [`hello`](../../hello.yaml) workflow (start -> template ->
  end) and waits for the final result (`RunHandle.waitJson`).
- **Stream events** prints every engine event of it as the run produces it
  (`EventListener.onEvent`, then `onFinish`).
- **Ask the assistant** runs [`agent.yaml`](../agent.yaml) with its scripted
  model ([`runtime-config.json`](../runtime-config.json)): the agent calls the
  app's native tool `device/battery_level`, asks in an alert before
  `device/send_message`, and streams `Done: sent to Ada` (see
  [`AssistantModel.swift`](GraphrtSample/AssistantModel.swift)).

`GraphrtSample.xcodeproj/project.pbxproj` is a small hand written project with
no generator. It references repository files in place:
`bindings/swift/graphrt_ffi.swift` and
`sdk/swift/Sources/Graphrt/Graphrt+Async.swift` (compiled into the app),
`examples/hello.yaml`, `examples/mobile/agent.yaml` and
`examples/mobile/runtime-config.json` (bundled as resources), and
`build/GraphrtFFI.xcframework` (built by the script below, not checked in). An
app of your own adds the Swift package instead (see
[docs/mobile.md](../../../docs/mobile.md#install)).

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

The script runs the Swift package's `sdk/swift/scripts/build-xcframework.sh`
for the device and the arm64 simulator, with the SDK's features (`--features
sdk`, what the release ships) and the `min-size` profile. To change this, set
`GRAPHRT_FEATURES`, `GRAPHRT_TARGETS` (add
`x86_64-apple-ios` for Intel simulators and drop the `EXCLUDED_ARCHS` setting
from the project), or `GRAPHRT_PROFILE`. If the shell's active developer
directory is the Command Line Tools, the script switches `DEVELOPER_DIR` to
`/Applications/Xcode.app`.

Sizes measured with `--no-default-features` (before the sample used the SDK's
features), `min-size` and Rust 1.95:
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

Or let the smoke test install, launch and check it (as CI does). It uses the
booted simulator, or the one in `SIMULATOR_DEVICE`, and fails unless the
console shows the expected greeting:

```sh
examples/mobile/ios/scripts/smoke-test.sh       # from the repository root
```

CI first runs `scripts/boot-simulator.sh`, which creates a pinned simulator
(iOS 18.5, iPhone 16), boots it and waits until it can launch a system app,
retrying a stalled boot on a fresh device. It prints the UDID to pass on:

```sh
SIMULATOR_DEVICE="$(examples/mobile/ios/scripts/boot-simulator.sh)" \
  examples/mobile/ios/scripts/smoke-test.sh
```

## Run on an iPhone

With the device connected and unlocked (Developer Mode on), build the device
slice and a signed app, then install and launch it:

```sh
GRAPHRT_TARGETS=aarch64-apple-ios examples/mobile/ios/scripts/build-xcframework.sh
cd examples/mobile/ios
xcodebuild -project GraphrtSample.xcodeproj -scheme GraphrtSample -configuration Debug \
  -destination 'id=<device id>' -derivedDataPath build/DerivedData \
  -allowProvisioningUpdates DEVELOPMENT_TEAM=<your team id> build
xcrun devicectl list devices                          # the device id
xcrun devicectl device install app --device <device id> \
  build/DerivedData/Build/Products/Debug-iphoneos/GraphrtSample.app
xcrun devicectl device process launch --device <device id> --console \
  dev.graphrt.sample -- -name Grace -autorun YES
```

The `--` keeps `devicectl` from reading the app's arguments as its own. This
was verified on an iPhone 14 Pro Max with iOS 18.3.

The launch arguments are optional and become `UserDefaults` values:
`-name Grace` fills in the name, and `-autorun YES` presses both buttons once
the workflow has loaded.

## Expected output

On screen, under the buttons:

```text
graphrt 0.6.2 loaded workflow 'hello'
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
