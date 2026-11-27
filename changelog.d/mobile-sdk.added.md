- Mobile SDK: apps give agents native tools (`Runtime.registerToolProvider`, a
  Kotlin `suspend` / Swift `async` `ToolProvider` with JSON Schema checks, time
  limits, cancellation and errors the model reads), receive typed run events
  (`WorkflowHandle.start`, `RunEvent`) and answer what a paused run asks with
  `PausedRun` (`approve`, `reject`, `decideToolCall`, `answer`, `toJson` /
  `fromJson`). Skills, learning, the file workspace, photo checks, UI block
  validation and scripted models are in the FFI behind cargo features, all in
  the new `sdk` feature set.
- Releases attach an Android AAR (`graphrt-android-<version>.aar`, arm64-v8a,
  armeabi-v7a, x86_64) and a Swift package with an XCFramework
  (`graphrt-swift-<version>.zip`, `GraphrtFFI.xcframework.zip`). Kotlin helpers
  (`Flow` of events, `registerTools { }`) live in `sdk/android`, Swift helpers
  (`AsyncThrowingStream`, `registerTools`) in `sdk/swift`. Publishing the AAR to
  Maven Central is prepared and off until it is set up.
- docs/mobile.md is a developer guide whose Kotlin and Swift examples are
  compiled and run by the SDK tests, and CI checks that the bindings match the
  API.
