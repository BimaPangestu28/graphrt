# Running graphrt on phones and desktops

`crates/graphrt-ffi` exposes graphrt to Kotlin (Android), Swift (iOS, macOS),
Python and other languages through [UniFFI](https://mozilla.github.io/uniffi-rs/)
0.32. Workflows built on a platform run inside the app itself, offline-capable
except for the model / API calls they make.

## What runs on mobile

Everything in the embedded build avoids spawning processes (forbidden on iOS):

| Feature | Android | iOS | Notes |
| --- | --- | --- | --- |
| Engine, all control nodes, templates, pause/resume | yes | yes | pure Rust |
| `llm`, `agent`, `question-classifier`, `parameter-extractor` | yes | yes | OpenAI-compatible + Anthropic over rustls |
| `http-request` | yes | yes | SSRF guard on by default |
| `code` (JavaScript) | yes | yes | QuickJS interpreter, no JIT (App Store friendly) |
| `document-extractor`, `knowledge-retrieval` | yes | yes | memory / Qdrant / HTTP retrievers |
| WebAssembly plugins | yes | yes | wasmi interpreter, no JIT |
| MCP tools over HTTP | yes | yes | stdio MCP servers are rejected |
| Python code, process plugins, stdio MCP | no | no | need child processes |

## API

The surface is JSON-string based so it stays stable across languages:

```text
Runtime(configJson)                              // one per app
  .registerWasmPlugin(bytes) -> [kinds]
  .catalogJson()                                 // node catalog for builders
  .loadWorkflow(yamlOrJson) -> WorkflowHandle    // throws GraphrtException.InvalidWorkflow
WorkflowHandle
  .run(inputsJson, sysJson, listener) -> RunHandle
  .resume(snapshotJson, inputsJson, secretsJson, listener) -> RunHandle
  .catalogJson() / .warnings() / .name()
RunHandle
  .abort(reason) / .pause(reason) / .updateVariable(selector, valueJson)
  .isFinished() / .waitJson(timeoutMs)           // waitJson blocks: never on the UI thread
EventListener (you implement)
  onEvent(eventJson)          // every raw event, e.g. {"type":"node_chunk",...}
  onAnswerChunk(text)         // ordered answer text, streamed
  onFinish(resultJson)        // once: {"status":"succeeded"|"failed"|"aborted"|"paused", "answer", ...}
```

A paused result (`"status": "paused"`, e.g. human approval) carries a
`snapshot`; store it and pass it to `resume` later, even after an app restart.

Config JSON (all keys optional):

```json
{
  "providers": { "OPENAI_API_KEY": "sk-...", "OLLAMA_HOST": "http://10.0.2.2:11434" },
  "env": { "CRM_TOKEN": "..." },
  "limits": { "max_steps": 10000, "max_duration_ms": 120000, "default_node_timeout_ms": 60000 },
  "allow_private_network": false,
  "knowledge": { "...": "datasets, see graphrt-rag KnowledgeConfig" },
  "mcp_servers": { "mcpServers": { "docs": { "url": "https://mcp.example.com/mcp" } } },
  "wasm": { "fuel": 1000000000, "memory_bytes": 67108864 }
}
```

Listener callbacks run on graphrt's worker threads. Exceptions thrown inside a
callback are swallowed so they cannot break the run.

## Android (Kotlin)

```kotlin
import uniffi.graphrt_ffi.*

class ChatListener(private val onText: (String) -> Unit) : EventListener {
    override fun onEvent(eventJson: String) {}
    override fun onAnswerChunk(text: String) = onText(text)
    override fun onFinish(resultJson: String) { Log.i("graphrt", resultJson) }
}

// Off the main thread (Runtime() may perform MCP handshakes):
val runtime = Runtime("""{"providers": {"OPENAI_API_KEY": "$key"}}""")
val workflow = runtime.loadWorkflow(assets.open("chatbot.yaml").bufferedReader().readText())

val run = workflow.run("{}", """{"query": "What is a DAG?"}""", ChatListener { chunk ->
    mainHandler.post { answerView.append(chunk) }
})
// later: run.abort("user cancelled")
```

Gradle: copy `bindings/kotlin/uniffi/graphrt_ffi/graphrt_ffi.kt` into your
module, put the `.so` files under `src/main/jniLibs/<abi>/`, and add
`implementation("net.java.dev.jna:jna:5.17.0@aar")` (UniFFI's Kotlin bindings
use JNA).

## iOS (SwiftUI)

```swift
import SwiftUI

final class Listener: EventListener {
    let onText: (String) -> Void
    init(onText: @escaping (String) -> Void) { self.onText = onText }
    func onEvent(eventJson: String) {}
    func onAnswerChunk(text: String) { onText(text) }
    func onFinish(resultJson: String) { print(resultJson) }
}

@MainActor final class ChatModel: ObservableObject {
    @Published var answer = ""
    private var workflow: WorkflowHandle?

    func load(yaml: String, apiKey: String) async throws {
        workflow = try await Task.detached {
            let runtime = try Runtime(configJson: #"{"providers": {"OPENAI_API_KEY": "\#(apiKey)"}}"#)
            return try runtime.loadWorkflow(document: yaml)
        }.value
    }

    func ask(_ query: String) throws {
        answer = ""
        let sys = try String(data: JSONEncoder().encode(["query": query]), encoding: .utf8) ?? "{}"
        _ = try workflow?.run(inputsJson: "{}", sysJson: sys, listener: Listener { chunk in
            Task { @MainActor in self.answer += chunk }
        })
    }
}
```

Add `bindings/swift/graphrt_ffi.swift` to the app target and link the
XCFramework built below (it contains `graphrt_ffiFFI.h` and the module map).

## Building

### Android

Requirements: Android NDK r27+ and `cargo-ndk`, plus `libclang` on the build
host (QuickJS bindings are generated at build time for mobile targets).

```sh
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
cargo install cargo-ndk
export ANDROID_NDK_HOME=/path/to/android-ndk-r28c
cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 -o app/src/main/jniLibs \
  build -p graphrt-ffi --profile min-size --features mobile-js
```

Measured sizes of `libgraphrt_ffi.so` (`min-size` profile, all default
features + `mobile-js`, NDK r28c): arm64-v8a **12.6 MB**, armeabi-v7a
**8.0 MB**, x86_64 **14.6 MB** (`release` arm64-v8a: 14.8 MB). Disable
`doc` / `rag` / `code-js` for a smaller library.

### iOS

Requirements: macOS with Xcode (the C code in `ring` and QuickJS needs the
Apple SDK; it cannot be cross-compiled from Linux).

```sh
rustup target add aarch64-apple-ios aarch64-apple-ios-sim
cargo build -p graphrt-ffi --profile min-size --features mobile-js --target aarch64-apple-ios
cargo build -p graphrt-ffi --profile min-size --features mobile-js --target aarch64-apple-ios-sim
xcodebuild -create-xcframework \
  -library target/aarch64-apple-ios/min-size/libgraphrt_ffi.a -headers bindings/swift/headers \
  -library target/aarch64-apple-ios-sim/min-size/libgraphrt_ffi.a -headers bindings/swift/headers \
  -output GraphrtFFI.xcframework
```

(`bindings/swift/headers` = `graphrt_ffiFFI.h` + `module.modulemap`, the
latter being `graphrt_ffiFFI.modulemap` renamed.)

### Regenerating bindings

Bindings are generated from an unstripped library (the `release` and
`min-size` profiles strip the UniFFI metadata), so use a debug build:

```sh
cargo build -p graphrt-ffi
for lang in kotlin swift python; do
  cargo run -p graphrt-ffi --features bindgen-cli --bin uniffi-bindgen -- \
    generate --library target/debug/libgraphrt_ffi.so --language $lang --out-dir bindings/$lang
done
```

### CI (GitHub Actions)

Example jobs producing an XCFramework and Android libraries:

```yaml
jobs:
  ios:
    runs-on: macos-15
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { targets: "aarch64-apple-ios,aarch64-apple-ios-sim" }
      - run: |
          for t in aarch64-apple-ios aarch64-apple-ios-sim; do
            cargo build -p graphrt-ffi --profile min-size --features mobile-js --target $t
          done
          mkdir -p headers
          cp bindings/swift/graphrt_ffiFFI.h headers/
          cp bindings/swift/graphrt_ffiFFI.modulemap headers/module.modulemap
          xcodebuild -create-xcframework \
            -library target/aarch64-apple-ios/min-size/libgraphrt_ffi.a -headers headers \
            -library target/aarch64-apple-ios-sim/min-size/libgraphrt_ffi.a -headers headers \
            -output GraphrtFFI.xcframework
      - uses: actions/upload-artifact@v4
        with: { name: GraphrtFFI.xcframework, path: GraphrtFFI.xcframework }

  android:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { targets: "aarch64-linux-android,armv7-linux-androideabi,x86_64-linux-android" }
      - uses: nttld/setup-ndk@v1
        id: ndk
        with: { ndk-version: r28c }
      - run: cargo install cargo-ndk
      - run: cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 -o jniLibs build -p graphrt-ffi --profile min-size --features mobile-js
        env: { ANDROID_NDK_HOME: "${{ steps.ndk.outputs.ndk-path }}" }
      - uses: actions/upload-artifact@v4
        with: { name: jniLibs, path: jniLibs }
```

## Verification status

- Rust API tests (`cargo test -p graphrt-ffi`): runs, streaming callbacks,
  typed errors, pause/resume through JSON, abort, panicking listeners, WASM
  plugin registration.
- Real FFI boundary: the generated Python bindings were exercised against the
  Linux `cdylib` (callbacks and typed exceptions).
- Android: all three ABIs compile and link with NDK r28c; not yet run on a
  device or emulator.
- iOS: pure-Rust crates (`graphrt-core`, `graphrt-wasm`, `graphrt-doc`,
  `graphrt-rag`, `graphrt-llm` without HTTP) type-check for `aarch64-apple-ios`;
  the full library needs a macOS build (see CI snippet).
