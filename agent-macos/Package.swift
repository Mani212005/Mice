// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "MiceMacAgent",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "capture-test", targets: ["CaptureTest"]),
        .executable(name: "inject-test", targets: ["InjectTest"]),
        .executable(name: "input-test", targets: ["InputTest"]),
        .executable(name: "ax-test", targets: ["AXTest"]),
        .executable(name: "mice-mac-agent", targets: ["MiceMacAgent"]),
    ],
    targets: [
        .target(name: "MiceMacSupport"),
        .executableTarget(name: "CaptureTest", dependencies: ["MiceMacSupport"]),
        .executableTarget(name: "InjectTest", dependencies: ["MiceMacSupport"]),
        .executableTarget(name: "InputTest", dependencies: ["MiceMacSupport"]),
        .executableTarget(name: "AXTest", dependencies: ["MiceMacSupport"]),
        .executableTarget(name: "MiceMacAgent", dependencies: ["MiceMacSupport"]),
    ]
)
