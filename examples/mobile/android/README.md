# graphrt Android sample

A single-Activity Kotlin app (plain Views, no Compose) that embeds Graphrt
through the Android SDK ([`sdk/android/graphrt`](../../../sdk/android)) and runs
two offline workflows on the device. No network, no API key.

- **Run** starts the [`hello`](../../hello.yaml) workflow (start -> template ->
  end) and waits for the final result (`RunHandle.waitJson`).
- **Stream events** prints every engine event of it as the run produces it
  (`EventListener.onEvent`, then `onFinish`).
- **Ask the assistant** runs [`agent.yaml`](../agent.yaml) with its scripted
  model ([`runtime-config.json`](../runtime-config.json)): the agent calls the
  app's native tool `device/battery_level`, asks in a dialog before
  `device/send_message`, and streams `Done: sent to Ada` (see
  [`AgentDemo.kt`](app/src/main/java/dev/graphrt/sample/AgentDemo.kt)).

The app reuses repository files instead of copying them: it depends on the SDK
module `:graphrt` (binding, helpers and native libraries) and bundles the
workflows and the config as assets.

## Prerequisites

- Rust (stable, 1.89+) with the Android targets:
  `rustup target add aarch64-linux-android x86_64-linux-android`
- libclang on the build host (the JavaScript code node's bindings are
  generated at build time), for example `apt install libclang-dev`
- Android SDK with platform 36 and an NDK r27 to r29 (tested with 27.1 and 28.2, r30 cannot build the JavaScript code node yet)
- [`cargo-ndk`](https://github.com/bbqsrc/cargo-ndk): `cargo install cargo-ndk`
- JDK 17+ for Gradle. The Gradle wrapper (9.3.1) downloads Gradle, AGP 8.13
  and Kotlin 2.2 on first use.

## Build

From the repository root:

```sh
# 1. Cross-compile libgraphrt_ffi.so for arm64-v8a and x86_64 into sdk/android/graphrt/src/main/jniLibs
examples/mobile/android/scripts/build-rust.sh

# 2. Build the APK
cd examples/mobile/android
export ANDROID_HOME=~/Library/Android/sdk   # or write sdk.dir to local.properties
./gradlew assembleDebug
# -> app/build/outputs/apk/debug/app-debug.apk
```

`build-rust.sh` runs the SDK's `sdk/android/scripts/build-native.sh` for the
two ABIs of the app. It picks the newest NDK under `$ANDROID_HOME/ndk` unless
`ANDROID_NDK_HOME` is set, and builds with the SDK's features (`--features
sdk`, what the release AAR ships) and the `min-size` profile. To change it, set
`GRAPHRT_FEATURES`, `GRAPHRT_ABIS` (then also `graphrt.abis` in
`gradle.properties`) or `GRAPHRT_PROFILE`.

Measured `libgraphrt_ffi.so` sizes (stripped, `min-size`, `sdk` features, NDK
r27b): arm64-v8a **17.4 MB**, x86_64 **20.2 MB**. The debug APK with both ABIs
is 40 MB.

## Run

```sh
adb install -r app/build/outputs/apk/debug/app-debug.apk
adb shell am start -n dev.graphrt.sample/.MainActivity
```

Type a name, then tap **Run** or **Stream events**. Two optional intent extras
help on emulators and in CI: `--es name Grace` fills in the name, and
`--ez autorun true` presses both buttons once the workflow has loaded.

```sh
adb shell am start -n dev.graphrt.sample/.MainActivity --es name Grace --ez autorun true
adb logcat -s GraphrtSample:I
```

To check the result automatically (as CI does), run the smoke test against a
running emulator or device instead; it installs the APK, starts it with
autorun and fails unless the logcat output shows the expected greeting:

```sh
examples/mobile/android/scripts/smoke-test.sh   # from the repository root
```

## Expected output

On screen, under the buttons:

```text
graphrt 0.6.3 loaded workflow 'hello'
status: succeeded
greeting: Hello, Grace.
{"elapsed_ms":1,"outputs":{"greeting":"Hello, Grace."},...,"status":"succeeded","answer":""}

events:
event: run_started
event: node_started [start]
event: node_succeeded [start]
event: node_started [greet]
event: node_succeeded [greet]
event: node_started [end]
event: node_succeeded [end]
event: run_succeeded
finish: {"elapsed_ms":1,"outputs":{"greeting":"Hello, Grace."},...}
```

With **excited** ticked the greeting is `Hello, Grace!!!`. The same lines go
to logcat under the `GraphrtSample` tag. `answer` is empty because the hello
workflow ends in an `end` node; workflows with `answer` nodes stream their text
through `onAnswerChunk`.

## Threading

graphrt calls go through a single background executor: creating the
`Runtime` can do network handshakes when MCP servers are configured, and
`waitJson` blocks. Listener callbacks arrive on graphrt's worker threads, so the
app posts every UI update to the main looper.
