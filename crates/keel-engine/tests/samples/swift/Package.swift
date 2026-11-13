// swift-tools-version:6.0
import PackageDescription

let package = Package(
    name: "KeelSample",
    targets: [
        .target(name: "KeelSample"),
        .testTarget(name: "KeelSampleTests", dependencies: ["KeelSample"]),
    ]
)
