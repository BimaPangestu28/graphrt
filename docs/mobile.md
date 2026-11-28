# Graphrt in a mobile app

Graphrt runs inside an Android or iOS app: the agent, its tools, approvals and
streamed answers all happen on the phone, with no server of your own. Only the
model calls (or your proxy) leave the device. This guide shows how to add the
SDK to an app, give an agent native tools, ask the person before a tool acts,
and ship it.

The SDK has three layers, all generated from one Rust crate
(`crates/graphrt-ffi`, [UniFFI](https://mozilla.github.io/uniffi-rs/) 0.32):

| Layer | Android | iOS / macOS |
| --- | --- | --- |
| Native library | `libgraphrt_ffi.so` per ABI, in the AAR | `GraphrtFFI.xcframework` (device, simulator, macOS) |
| Generated binding | package `uniffi.graphrt_ffi` | the `Graphrt` module |
| Helpers | package `com.graphrt`: `Flow` of events, `registerTools { }` | `AsyncThrowingStream` of events, `registerTools { }` |

Every Kotlin and Swift example in this guide is compiled and run by the SDK's
tests (`sdk/android/graphrt-jvm` and `sdk/swift/Tests`), so the examples work
with the release they come from.

## Install

### Android

Each [GitHub release](https://github.com/Graphrt/graphrt/releases) has
`graphrt-android-<version>.aar` (arm64-v8a, armeabi-v7a, x86_64, Android 8.0 /
API 26 and newer). Put it in `app/libs/` and add:

```kotlin
// app/build.gradle.kts
dependencies {
    implementation(files("libs/graphrt-android-0.6.4.aar"))
    // What the AAR needs (a Maven Central release declares these itself):
    implementation("net.java.dev.jna:jna:5.17.0@aar")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.10.2")
}
```

The AAR carries R8 rules for JNA and the binding, so minified release builds
keep working. A Maven Central artifact (`com.graphrt:graphrt-android`) is
prepared but not published yet (see [Publishing](#publishing)).

### iOS

Each release has `graphrt-swift-<version>.zip`, a Swift package whose
`Package.swift` points at the release's `GraphrtFFI.xcframework.zip` by URL and
checksum. Unzip it next to your project (or push it to a git repository of
your own) and add it in Xcode with **File > Add Package Dependencies > Add
Local**, then `import Graphrt`. iOS 16 and macOS 13 are the minimum.

### From this repository

```sh
sdk/android/scripts/build-native.sh       # .so files into sdk/android/graphrt
(cd sdk/android && ./gradlew :graphrt:assembleRelease)
sdk/swift/scripts/build-xcframework.sh    # sdk/swift/GraphrtFFI.xcframework (macOS with Xcode)
```

A Gradle build can include `sdk/android/graphrt` as a module, as
[the Android sample](../examples/mobile/android/settings.gradle.kts) does, and
Xcode can add `sdk/swift` as a local package.

## Quick start

The examples use the agent of the sample apps,
[`examples/mobile/agent.yaml`](../examples/mobile/agent.yaml): an `agent` node
with the tools `device/battery_level` and `device/send_message`, where
`send_message` is listed in `require_approval`, and an `answer` node that
streams the agent's text.

### 1. A runtime with native tools

Create one runtime per app, off the main thread, and register the app's tools
before loading the workflows that use them. A tool is a name, a description
for the model, a JSON Schema of its arguments and a suspend (Kotlin) or async
(Swift) function.

<!-- snippet: kotlin-runtime -->
```kotlin
// Off the main thread: creating a runtime may connect to MCP servers.
val runtime = Runtime(configJson)
runtime.registerTools("device") {
    tool("battery_level", "The battery level of the phone, in percent.") {
        toolResult("${phone.batteryPercent()} %")
    }
    tool(
        name = "send_message",
        description = "Send a text message.",
        parametersJson = """
            {"type": "object",
             "properties": {"to": {"type": "string"}, "text": {"type": "string"}},
             "required": ["to", "text"]}
        """,
        timeoutMs = 10_000u,
    ) { argumentsJson ->
        val args = Json.parseObject(argumentsJson)
        if (!phone.sendSms(args.getValue("to"), args.getValue("text"))) {
            throw ToolCallException.Failed("the SMS could not be sent")
        }
        toolResult("sent to ${args.getValue("to")}")
    }
}
```

<!-- snippet: swift-runtime -->
```swift
// Off the main actor: creating a runtime may connect to MCP servers.
let runtime = try Runtime(configJson: configJson)
try runtime.registerTools(provider: "device") { tools in
    tools.tool("battery_level", description: "The battery level of the phone, in percent.") { _ in
        toolResult("\(phone.batteryPercent()) %")
    }
    tools.tool(
        "send_message",
        description: "Send a text message.",
        parametersJson: """
            {"type": "object",
             "properties": {"to": {"type": "string"}, "text": {"type": "string"}},
             "required": ["to", "text"]}
            """,
        timeoutMs: 10_000
    ) { argumentsJson in
        let args = try JSONDecoder().decode([String: String].self, from: Data(argumentsJson.utf8))
        guard let to = args["to"], phone.sendSms(to: to, text: args["text"] ?? "") else {
            throw ToolCallError.Failed(reason: "the SMS could not be sent")
        }
        return toolResult("sent to \(to)")
    }
}
```

The runtime takes care of everything around a call:

- The arguments are checked against the schema (`type`, `required`,
  `properties`, `enum`, `items`, `additionalProperties: false`) before your
  function runs. A call that does not match goes back to the model as an error,
  so it can fix the call, and your function never sees it.
- Every call has a time limit (`timeoutMs`, 30 s by default, at most 10
  minutes). When it passes, or the run is aborted, the call is cancelled: the
  Kotlin coroutine or the Swift task receives a cancellation.
- `ToolCallException.Failed` (Kotlin) or `ToolCallError.Failed` (Swift)
  becomes an error result the model reads, with your reason. Any other
  exception becomes the same, with its text. Neither crashes the run.
- `text` and `dataJson` are limited to 1 MiB each. UI blocks in `dataJson`
  (`{"$ui": "stats", ...}`) reach the app checked, as `RunEvent.Ui`.

Agents list the tools as `device/battery_level`, or `device/*` for all of
them. A provider name is unique per runtime: it cannot reuse the name of an MCP
server, of the `files` workspace or of another provider.

### 2. Run, ask the person, resume

`events` starts a run and returns its typed events as a `Flow` (Kotlin) or an
`AsyncThrowingStream` (Swift). The stream ends with `Paused` or `Finished`.
Cancelling the collector, for example when the screen goes away, aborts the
run.

<!-- snippet: kotlin-run -->
```kotlin
var events = workflow.events("""{"query": "Tell Ada my battery level"}""")
while (true) {
    var paused: PausedRun? = null
    events.collect { event ->
        when (event) {
            is RunEvent.AnswerChunk -> screen.appendAnswer(event.text)
            is RunEvent.ToolCalled -> screen.showToolActivity(event.name)
            is RunEvent.Ui -> screen.showBlock(event.block)
            is RunEvent.Paused -> paused = event.run
            is RunEvent.Finished -> event.result.error?.let(screen::showError)
            else -> {}
        }
    }
    val run = paused ?: return RunStatus.SUCCEEDED
    // The run waits for a person: ask, answer every request, resume.
    for (request in run.requests()) {
        if (request.kind != RequestKind.ToolApproval) continue
        val call = request.toolCalls.first()
        if (screen.askToApprove(call.name, call.argumentsJson)) {
            run.approve(request.key, null)
        } else {
            run.reject(request.key, "The user said no.")
        }
    }
    events = workflow.resumeEvents(run)
}
```

<!-- snippet: swift-run -->
```swift
var events = workflow.events(inputsJson: #"{"query": "Tell Ada my battery level"}"#)
while true {
    var paused: PausedRun?
    for try await event in events {
        switch event {
        case .answerChunk(let text): screen.appendAnswer(text)
        case .toolCalled(_, _, let name, _): screen.showToolActivity(name)
        case .ui(_, let block): screen.showBlock(block)
        case .paused(let run): paused = run
        case .finished(let result): if let error = result.error { screen.showError(error) }
        default: break
        }
    }
    guard let run = paused else { return .succeeded }
    // The run waits for a person: ask, answer every request, resume.
    for request in run.requests() where request.kind == .toolApproval {
        guard let call = request.toolCalls.first else { continue }
        if await screen.askToApprove(toolName: call.name, argumentsJson: call.argumentsJson) {
            try run.approve(key: request.key, reason: nil)
        } else {
            try run.reject(key: request.key, reason: "The user said no.")
        }
    }
    events = workflow.resumeEvents(paused: run)
}
```

The events:

| Event | When |
| --- | --- |
| `Started` | the run started (`resumed` after a resume) |
| `NodeStarted`, `NodeSucceeded`, `NodeFailed` | a node's progress, for a progress view |
| `ToolCalled`, `ToolFinished`, `ToolDenied` | an agent called a tool, it finished (`isError`), or a held call was rejected |
| `AnswerChunk` | the next piece of the answer, in order |
| `Ui` / `UiRejected` | a UI block of a node's outputs that passed validation, or one that did not (never render that one) |
| `Paused` | the run waits for answers: a `PausedRun`. Terminal |
| `Finished` | a `RunResult`: `status`, `answer`, `outputsJson`, `error`, `errorType`. Terminal |

A `PausedRun` lists its `requests()`. Each has a `key` to answer under and a
`kind`:

| Kind | What it holds | How to answer |
| --- | --- | --- |
| `ToolApproval` | `toolCalls`: id, name and arguments of each held call | `approve(key, reason)`, `reject(key, reason)`, or `decideToolCall(key, callId, approved)` per call |
| `HumanInput` (a `human-input` node) | `prompt`, `fields`, `actions`, `timeoutMs`, `cardJson` | `answer(key, action, valuesJson)`, or `approve` / `reject` for forms with those actions |
| `Other` | `dataJson` | `setInput(key, inputJson)` |

A rejected call never reaches your function. The model is told the person did
not approve it, with your reason, and continues. Requests left unanswered keep
their nodes paused after the resume.

### 3. Keep a paused run

A paused run can wait for hours, for instance until a push notification
brings the person back. Store it and restore it, even after the app was
killed:

<!-- snippet: kotlin-persist -->
```kotlin
// Keep the paused run in the app's private storage, for instance until a
// push notification brings the person back.
storage["paused:${paused.runId()}"] = paused.toJson()

// Later, even after the app was restarted:
val restored = PausedRun.fromJson(storage.getValue("paused:${paused.runId()}"))
```

<!-- snippet: swift-persist -->
```swift
// Keep the paused run in the app's private storage, for instance until a
// push notification brings the person back.
storage["paused:\(paused.runId())"] = paused.toJson()

// Later, even after the app was restarted:
let restored = try PausedRun.fromJson(json: storage["paused:\(paused.runId())"] ?? "")
```

The stored text holds the run's variables (never secrets): keep it in the
app's private storage. Load the same workflow document before resuming, the
snapshot is checked against it.

## Capabilities

The runtime config (a JSON object, every key optional) switches on what a
workflow may use:

```json
{
  "credentials": { "openai": { "source": "external" } },
  "env": { "CRM_TOKEN": "..." },
  "limits": { "max_steps": 10000, "max_duration_ms": 120000, "default_node_timeout_ms": 60000 },
  "allow_private_network": false,
  "mcp_servers": { "mcpServers": { "docs": { "url": "https://mcp.example.com/mcp" } } },
  "knowledge": { "...": "datasets, see graphrt-rag KnowledgeConfig" },
  "skills_dir": "<app files>/graphrt/skills",
  "learning_dir": "<app files>/graphrt/learning",
  "workspace": { "dir": "<app files>/graphrt/workspace", "read_only": false, "max_total_bytes": 104857600 },
  "scripted_models": { "demo": [{ "text": "Hello from a scripted model." }] },
  "wasm": { "fuel": 1000000000, "memory_bytes": 67108864 }
}
```

Use `context.filesDir` (Android) or the Application Support directory (iOS)
for the directories. Unknown keys are refused, so a typo fails at start.

### Skills and learning

With `skills_dir`, an `agent` node's `skills` field loads skills you install
from their `SKILL.md` text (instructions only: no code of a skill ever runs).
With `learning_dir`, a node's `learning` field adds approved lessons to the
system prompt. Lessons come from what people say about a run and are used only
once the owner approved them.

<!-- snippet: kotlin-skills-learning -->
```kotlin
// Needs `skills_dir` and `learning_dir` in the runtime config.
runtime.installSkill(
    SkillInstall(
        skillMd = """
            ---
            name: trip-planner
            description: Use when someone plans a trip.
            ---
            Ask for the dates and the budget before suggesting anything.
        """.trimIndent(),
    ),
)

// A person corrected the agent: propose a lesson, the owner decides.
val lesson = runtime.proposeLesson(
    LessonProposal(
        agent = "assistant",
        user = "ada",
        kind = LessonKind.PREFERENCE,
        text = "Answer in Indonesian.",
        authorAuthenticated = true,
    ),
)
runtime.decideLesson("assistant", lesson.id, LessonDecision.APPROVE)
val approved = runtime.listLessons("assistant", LessonStatus.APPROVED).map { it.text }
```

<!-- snippet: swift-skills-learning -->
```swift
// Needs `skills_dir` and `learning_dir` in the runtime config.
_ = try runtime.installSkill(request: SkillInstall(skillMd: """
    ---
    name: trip-planner
    description: Use when someone plans a trip.
    ---
    Ask for the dates and the budget before suggesting anything.
    """))

// A person corrected the agent: propose a lesson, the owner decides.
let lesson = try runtime.proposeLesson(proposal: LessonProposal(
    agent: "assistant",
    user: "ada",
    kind: .preference,
    text: "Answer in Indonesian.",
    authorAuthenticated: true
))
_ = try runtime.decideLesson(agent: "assistant", id: lesson.id, decision: .approve)
let approved = try runtime.listLessons(agent: "assistant", status: .approved).map(\.text)
```

`listSkills`, `setSkillEnabled`, `removeSkill`, `listLessons`, `addLesson`
(the owner's own, approved at once) and `deleteLesson` complete the set. See
[skills.md](skills.md) and [learning.md](learning.md) for the formats.

### Files

`workspace` gives agents the `files` tools (`files/list`, `files/read`,
`files/write`, `files/search`...) inside that one directory, with quotas (32 MiB
per file, 512 MiB and 5,000 files in total by default) and no executables. With
`read_only` agents can only read. See [files-and-images.md](files-and-images.md).

### Images

Photos a model node sends (`vision` inputs) are checked first: the real type
must be an image, size and pixels are bounded, EXIF data (GPS position, device,
time) is removed and large photos are scaled down. Nothing to configure.

### UI blocks

UI blocks in a run (cards, charts, tables, stat tiles, diagrams) arrive
checked as `RunEvent.Ui`. Check blocks from anywhere else before rendering
them:

<!-- snippet: kotlin-ui -->
```kotlin
// Blocks inside a run arrive checked as RunEvent.Ui. Check blocks from
// anywhere else (a server, stored messages) before rendering them:
val block = try {
    validateUiBlock(json)
} catch (e: Exception) {
    null // never render a block that failed validation
}
```

<!-- snippet: swift-ui -->
```swift
// Blocks inside a run arrive checked as RunEvent.ui. Check blocks from
// anywhere else (a server, stored messages) before rendering them:
let block = try? validateUiBlock(blockJson: json) // nil: never render it
```

### Scripted models

`scripted_models` registers model providers that answer from a script, with
no network and no key: for UI tests, previews and demos. A request gets the
step with the number of assistant turns since the last user message, so the
same script plays out the same way for every conversation, across pauses. A
step is `{"text": "..."}` (streamed word by word, `{tool_result}` is replaced
with the latest tool result) or `{"tool_calls": [{"name": "...", "arguments":
{...}}]}`. [`examples/mobile/runtime-config.json`](../examples/mobile/runtime-config.json)
is the script of the samples.

### Cargo features

Every function and type of the binding exists in every build. A capability
that is compiled out fails with `GraphrtException.Unsupported` /
`GraphrtError.Unsupported`, so one binding fits every build. The release
artifacts are built with `sdk`:

| Feature | Adds | In `sdk` |
| --- | --- | --- |
| `code-js` + `mobile-js` | the JavaScript `code` node (QuickJS, an interpreter, no JIT) | yes |
| `mcp-http` | MCP tool servers over streamable HTTP | yes |
| `doc` | the `document-extractor` node (PDF, Office, EPUB, email) | yes |
| `rag` | the `knowledge-retrieval` node (memory, SQLite, Qdrant, HTTP) | yes |
| `skills` | `skills_dir`, `installSkill`... | yes |
| `learning` | `learning_dir`, `proposeLesson`... | yes |
| `files` | `workspace` and the `files` tools | yes |
| `image` | the photo checks above | yes |
| `ui` | `validateUiBlock` and `RunEvent.Ui` | yes |
| `scripted-llm` | `scripted_models` | yes |

The engine, every control node, templates, pause and resume, the `llm`,
`agent`, `question-classifier`, `parameter-extractor` and `http-request` nodes
(with the SSRF guard on), WebAssembly plugins (the wasmi interpreter) and
credentials are always in.

### What stays on desktops and servers

| Capability | Why not in an app |
| --- | --- |
| Process plugins, `code` in Python | they start child processes, which iOS forbids and Android kills in the background |
| MCP servers over stdio | the same: a stdio server is a child process. Use MCP over HTTP |
| Schedules, triggers and webhooks | an app is suspended in the background, so a timer cannot be relied on. Run scheduled agents with `graphrt serve` and send the phone a push notification |
| `graphrt serve`, the gateway, deployments | they listen on ports for other clients. To use a deployed agent from a phone, see [web.md](web.md) |
| Bundles with process or HTTP plugins | `loadBundle` refuses them for the reasons above. WebAssembly plugins load |

## Credentials

Never ship a provider key in an app: anything in an APK or IPA can be
extracted. Declare providers in `credentials` and supply keys at run time (full
guide: [credentials.md](credentials.md)):

| Pattern | Config entry | The app's `CredentialProvider` returns |
| --- | --- | --- |
| Bring your own key | `"openai": {"source": "external"}` | the user's key from Keychain / Keystore |
| Thin proxy | `"proxy": {"source": "external", "kind": "openai", "base_url": "https://llm-proxy.example.com/v1"}` | the user's session token (verified by your proxy) |
| Token broker | `"proxy": {"source": "token_broker", "url": "https://llm-proxy.example.com/token", "app_auth": "external", "kind": "openai", "base_url": "https://llm-proxy.example.com/v1"}` | the session token, asked as provider `token_broker`. Graphrt fetches and refreshes short lived model tokens itself |

`getCredential` runs on a background thread and may block (Keychain, IPC,
network). Results are cached until `expiresAtMs` minus 60 s, or until the
provider answers 401 or 403: then `invalidate` is called and `getCredential` is
asked once more. Errors from the callback fail only that model call.

### Android: Keystore backed storage

```kotlin
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKey
import uniffi.graphrt_ffi.*

/** Bring your own key: the user's key, encrypted with a Keystore held master key. */
class KeystoreCredentials(context: Context) : CredentialProvider {
    private val prefs = EncryptedSharedPreferences.create(
        context, "llm_keys",
        MasterKey.Builder(context).setKeyScheme(MasterKey.KeyScheme.AES256_GCM).build(),
        EncryptedSharedPreferences.PrefKeyEncryptionScheme.AES256_SIV,
        EncryptedSharedPreferences.PrefValueEncryptionScheme.AES256_GCM,
    )

    override fun getCredential(provider: String): CredentialRecord {
        val key = prefs.getString("key_$provider", null)
            ?: throw GraphrtException.InvalidInput("no API key saved for $provider")
        return CredentialRecord(secret = key, expiresAtMs = null, headersJson = null, baseUrl = null)
    }

    override fun invalidate(provider: String) {
        // The provider rejected the key: ask the user to enter it again.
        prefs.edit().remove("key_$provider").apply()
    }
}

val runtime = Runtime("""{"credentials": {"openai": {"source": "external"}}}""")
runtime.setCredentialProvider("*", KeystoreCredentials(applicationContext))
```

### iOS: Keychain

```swift
import Security

/// Bring your own key from the Keychain. For a token broker, return the user's
/// session token when `provider == "token_broker"` instead.
final class KeychainCredentials: CredentialProvider {
    func getCredential(provider: String) throws -> CredentialRecord {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: "llm-keys",
            kSecAttrAccount as String: provider,
            kSecReturnData as String: true,
        ]
        var item: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess,
              let data = item as? Data, let key = String(data: data, encoding: .utf8) else {
            throw GraphrtError.InvalidInput(message: "no API key saved for \(provider)")
        }
        return CredentialRecord(secret: key, expiresAtMs: nil, headersJson: nil, baseUrl: nil)
    }

    func invalidate(provider: String) {
        // The provider rejected the key: prompt the user on the main actor.
    }
}

let runtime = try Runtime(configJson: #"{"credentials": {"openai": {"source": "external"}}}"#)
runtime.setCredentialProvider(provider: "*", callback: KeychainCredentials())
```

Server examples for the proxy and the broker: [`examples/proxy/`](../examples/proxy/).

## Threads and errors

- `Runtime(...)`, `loadWorkflow` and `waitJson` may block: call them off the
  main thread. `events` and `resumeEvents` return at once.
- Listeners and tools are called on Graphrt's threads. The `Flow` and the
  stream deliver events on the collector's context, so collect on the main
  dispatcher (or hop to the main actor) to touch the UI.
- Errors are `GraphrtException` subclasses in Kotlin and `GraphrtError` cases
  in Swift: `InvalidConfig`, `InvalidWorkflow`, `InvalidInput`, `Plugin`,
  `Runtime`, `Unsupported`. The text is in `detail` in Kotlin (`message` would
  clash with `Throwable.message`) and in `message` in Swift and Python.

## Lower level API

The helpers wrap the generated binding, which stays available:

```text
Runtime(configJson)
  .registerToolProvider(name, [HostToolSpec], ToolProvider)
  .registerWasmPlugin(bytes) / .loadBundle(bytes, trustedKeys)
  .loadWorkflow(yamlOrJson) -> WorkflowHandle
  .setCredentialProvider(provider, callback) / .clearCredentialProvider / .clearCredentialCache
  .catalogJson() / .toolProviders()
WorkflowHandle
  .start(inputsJson, sysJson, RunListener) -> RunHandle             typed events
  .resumePaused(PausedRun, secretsJson, RunListener) -> RunHandle
  .run(inputsJson, sysJson, EventListener) -> RunHandle             raw JSON events
  .resume(snapshotJson, inputsJson, secretsJson, EventListener) -> RunHandle
RunHandle
  .abort(reason) / .pause(reason) / .updateVariable(selector, valueJson)
  .isFinished() / .waitJson(timeoutMs)
PausedRun.fromJson(json)                                            also reads a waitJson result
validateUiBlock(json) / graphrtVersion()
```

`EventListener` (`onEvent(eventJson)`, `onAnswerChunk(text)`,
`onFinish(resultJson)`) is the JSON based listener of earlier releases. It
keeps working.

## Sample apps

[`examples/mobile/android`](../examples/mobile/android/README.md) (Kotlin,
Views) and [`examples/mobile/ios`](../examples/mobile/ios/README.md)
(SwiftUI) run the offline [`hello`](../examples/hello.yaml) workflow and the
phone assistant above with its scripted model: **Ask the assistant** calls
`battery_level`, asks before `send_message` and streams `Done: sent to Ada`.
Each has `scripts/smoke-test.sh`, which installs the app on a running
emulator, simulator or device, presses every button with autorun (approving
the request) and fails unless every step shows up in the log.

## Sizes

`libgraphrt_ffi.so` with the `sdk` features, `min-size` profile, NDK r27b,
stripped: arm64-v8a **17.4 MB**, armeabi-v7a **11.2 MB**, x86_64 **20.2 MB**.
The AAR with all three is 22.4 MB (compressed). Google Play delivers one ABI
per phone. A build with fewer features is smaller: drop `doc`, `rag` or
`code-js` (for example `GRAPHRT_FEATURES="--no-default-features --features
skills,learning,ui"`) and rebuild the AAR.

## Testing

| What | Where | Runs |
| --- | --- | --- |
| Rust API tests | `crates/graphrt-ffi/tests` | `cargo test -p graphrt-ffi --features sdk` |
| Kotlin SDK tests and the Kotlin snippets of this guide, on the JVM | `sdk/android/graphrt-jvm` | `sdk/android/scripts/test-jvm.sh` |
| Swift SDK tests and the Swift snippets | `sdk/swift/Tests` | `swift test` in `sdk/swift` on a Mac (after `build-xcframework.sh`), or on Linux against the host library, as CI does |
| The guide's snippets are the tested code | `scripts/check-doc-snippets.py` | `python3 scripts/check-doc-snippets.py` |
| The bindings match the API | `scripts/generate-bindings.sh --check` | after a change to `graphrt-ffi`, run it without `--check` and commit `bindings/` |
| The samples in an emulator or simulator | `examples/mobile/*/scripts/smoke-test.sh` | see the samples' READMEs |

In CI ([`mobile.yml`](../.github/workflows/mobile.yml)) the `sdk` job runs the
bindings check, the snippet check and the Kotlin and Swift tests on Linux, and
the `android` job runs the Android sample in an API 34 x86_64 emulator, on
every pull request that touches the crates, the bindings, the SDKs or the
samples. The `ios` job builds the XCFramework, runs `swift test` on macOS and
the iOS sample in an iOS 18.5 simulator on `main`, every night and on pull
requests with the `macos` label.

### Testing on devices

CI covers emulators and simulators only. Before a release, run the samples on
real hardware:

**Android** (an arm64 phone, Android 8.0 or newer): turn on USB debugging,
run `examples/mobile/android/scripts/build-rust.sh`, `./gradlew assembleDebug`
in `examples/mobile/android`, then `scripts/smoke-test.sh` (set
`ANDROID_SERIAL` when an emulator also runs). Then tap **Ask the assistant**
by hand, answer the dialog both ways, rotate the phone and leave and reopen the
app during a run.

**iOS** (an iPhone with iOS 16 or newer, Developer Mode on): build the device
slice and a signed app as in
[the iOS sample](../examples/mobile/ios/README.md#run-on-an-iphone), install
with `xcrun devicectl device install app`, launch with `xcrun devicectl device
process launch --console --device <id> dev.graphrt.sample -- -name Grace
-autorun YES` and check the same lines as the smoke test.

## Publishing

The release workflow ([`release.yml`](../.github/workflows/release.yml))
attaches `graphrt-android-<version>.aar`, `GraphrtFFI.xcframework.zip` and
`graphrt-swift-<version>.zip` to every GitHub release, with SHA256SUMS.

The `maven-central` job is off until the owner sets it up:

1. A Central Portal account at <https://central.sonatype.com> and the
   namespace `com.graphrt`, verified with the DNS TXT record the portal shows,
   added to `graphrt.com` (its DNS is at Cloudflare). The artifact is then
   `com.graphrt:graphrt-android`.
2. A PGP key for signing, its public key on `keys.openpgp.org` or another key
   server Central reads.
3. Repository secrets `MAVEN_CENTRAL_USERNAME` and `MAVEN_CENTRAL_PASSWORD` (a
   Central Portal user token), `SIGNING_KEY` (the ASCII armored private key)
   and `SIGNING_PASSWORD`, in a `maven-central` environment.
4. The repository variable `PUBLISH_MAVEN=true`.

The upload is user managed: it waits in the Central Portal until someone
presses **Publish**. A Swift package registry or a separate `graphrt-swift`
repository (so apps can add the package by git URL) are possible later steps
and need no change to the artifacts.
