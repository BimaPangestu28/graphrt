# Graphrt for Android

The Android SDK of Graphrt: run agent workflows inside an app, with native
tools, approvals and streamed answers. The guide is
[docs/mobile.md](../../docs/mobile.md).

| Module | What it is |
| --- | --- |
| `graphrt` | the AAR: the Kotlin binding (`uniffi.graphrt_ffi`), the helpers in `com.graphrt` (`WorkflowHandle.events`, `Runtime.registerTools`...) and `libgraphrt_ffi.so` for arm64-v8a, armeabi-v7a and x86_64 |
| `graphrt-jvm` | the same Kotlin sources compiled for the JVM, with the SDK's tests and the Kotlin snippets of the guide, run against a host build of the library |

```sh
scripts/build-native.sh                  # the .so files, with the `sdk` features
./gradlew :graphrt:assembleRelease       # graphrt/build/outputs/aar/graphrt-release.aar
scripts/test-jvm.sh                      # tests and snippets on the JVM
```

Needs the Android NDK r27 to r29, `cargo-ndk`, libclang and a JDK 17+. The release
workflow builds the AAR and attaches it to every GitHub release.
