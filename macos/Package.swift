// swift-tools-version:5.10
import PackageDescription

// The Rust core is linked as a static library built by build.sh into ../target/universal.
let rustLib = Context.packageDirectory + "/../target/universal"

let package = Package(
    name: "KVM",
    platforms: [.macOS(.v13)],
    targets: [
        .systemLibrary(name: "CKvm", path: "Sources/CKvm"),
        .target(name: "CGVirtualDisplay", path: "Sources/CGVirtualDisplay"),
        .executableTarget(
            name: "KVM",
            dependencies: ["CKvm", "CGVirtualDisplay"],
            linkerSettings: [
                .unsafeFlags(["-L", rustLib]),
                .linkedFramework("CoreGraphics"),
                .linkedFramework("Security"),
                .linkedFramework("SystemConfiguration"),
                .linkedFramework("CoreWLAN"),
                .linkedFramework("IOKit"),
            ]
        ),
    ]
)
