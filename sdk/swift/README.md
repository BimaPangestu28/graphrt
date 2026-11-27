# Graphrt for Swift

The Swift package of Graphrt (iOS 16+, macOS 13+): run agent workflows inside
an app, with native tools, approvals and streamed answers. The guide is
[docs/mobile.md](https://github.com/Graphrt/graphrt/blob/main/docs/mobile.md).

`import Graphrt` gives the generated binding (`Runtime`, `WorkflowHandle`,
`PausedRun`...) and the helpers in `Graphrt+Async.swift`
(`WorkflowHandle.events`, `Runtime.registerTools`...). The native library is
the binary target `GraphrtFFI.xcframework`.

In the Graphrt repository:

```sh
scripts/build-xcframework.sh    # GraphrtFFI.xcframework (needs Xcode)
swift test                      # the tests and the Swift snippets of the guide
```

Each GitHub release attaches this package as `graphrt-swift-<version>.zip`,
with `Package.swift` pointing at the release's `GraphrtFFI.xcframework.zip`.
