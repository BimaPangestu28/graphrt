# graphrt Android sample

A single-Activity Kotlin app (plain Views, no Compose) that embeds graphrt
through the UniFFI Kotlin binding and runs the offline
[`examples/hello.yaml`](../../hello.yaml) workflow (start → template → end)
on the device. No network, no API key.

- **Run** starts a run and waits for the final result (`RunHandle.waitJson`).
- **Stream events** prints every engine event as the run produces it
  (`EventListener.onEvent`, then `onFinish`).

The app reuses repository files instead of copying them: Gradle compiles
`bindings/kotlin/uniffi/graphrt_ffi/graphrt_ffi.kt` in place and bundles
`examples/hello.yaml` as the asset `hello.yaml`.

## Prerequisites

- Rust (stable, 1.86+) with the Android targets:
  `rustup target add aarch64-linux-android x86_64-linux-android`
- Android SDK with platform 36 and an NDK (r27+; tested with 28.2.13676358)
- [`cargo-ndk`](https://github.com/bbqsrc/cargo-ndk): `cargo install cargo-ndk`
- JDK 17+ for Gradle. The Gradle wrapper (9.3.1) downloads Gradle, AGP 8.13
  and Kotlin 2.2 on first use.

## Build

From the repository root:

```sh
# 1. Cross-compile libgraphrt_ffi.so for arm64-v8a and x86_64 into app/src/main/jniLibs
examples/mobile/android/scripts/build-rust.sh

# 2. Build the APK
cd examples/mobile/android
export ANDROID_HOME=~/Library/Android/sdk   # or write sdk.dir to local.properties
./gradlew assembleDebug
# -> app/build/outputs/apk/debug/app-debug.apk
```

`build-rust.sh` picks the newest NDK under `$ANDROID_HOME/ndk` unless
`ANDROID_NDK_HOME` is set. It builds with `--no-default-features` and the
`min-size` profile: the engine, templates and the LLM / HTTP nodes stay, while
the JavaScript code node, MCP, document extraction and RAG are left out. That
is all the hello workflow needs. To change it, set `GRAPHRT_FEATURES`, for
example `GRAPHRT_FEATURES="--features mobile-js"` (needs libclang), or
`GRAPHRT_ABIS`, or `GRAPHRT_PROFILE`.

Measured `libgraphrt_ffi.so` sizes (stripped, `min-size`,
`--no-default-features`, NDK 28.2, Rust 1.95): arm64-v8a **8.5 MB**,
x86_64 **10.2 MB**. The debug APK with both ABIs is 20 MB.

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

## Expected output

On screen, under the buttons:

```text
graphrt 0.3.0 loaded workflow 'hello'
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
