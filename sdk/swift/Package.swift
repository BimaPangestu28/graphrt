// swift-tools-version:5.9
// The Swift package of Graphrt: the generated binding, Swift helpers and the
// native library as an XCFramework (iOS device, iOS simulator, macOS).
//
// In this repository the XCFramework comes from scripts/build-xcframework.sh;
// the package attached to a GitHub release points at the release's
// GraphrtFFI.xcframework.zip instead (scripts/package-release.sh).
// On Linux the native library is a system library, for the tests only.
import PackageDescription

#if os(Linux)
let nativeTarget: Target = .systemLibrary(name: "graphrt_ffiFFI", path: "Sources/graphrt_ffiFFI")
let nativeDependency: Target.Dependency = "graphrt_ffiFFI"
#else
let nativeTarget: Target = .binaryTarget(name: "GraphrtFFI", path: "GraphrtFFI.xcframework")
let nativeDependency: Target.Dependency = "GraphrtFFI"
#endif

let package = Package(
    name: "Graphrt",
    platforms: [.iOS(.v16), .macOS(.v13)],
    products: [
        .library(name: "Graphrt", targets: ["Graphrt"]),
    ],
    targets: [
        nativeTarget,
        .target(
            name: "Graphrt",
            dependencies: [nativeDependency],
            path: "Sources/Graphrt",
            linkerSettings: [
                // TLS certificate checks and network reachability of the HTTP client.
                .linkedFramework("Security", .when(platforms: [.iOS, .macOS])),
                .linkedFramework("SystemConfiguration", .when(platforms: [.iOS, .macOS])),
            ]
        ),
        .testTarget(
            name: "GraphrtTests",
            dependencies: ["Graphrt"],
            path: "Tests/GraphrtTests"
        ),
    ]
)
