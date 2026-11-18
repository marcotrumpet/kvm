// swift-tools-version:5.10
import PackageDescription

// The Rust core is linked as a static library built by build.sh into ../target/universal.
let rustLib = Context.packageDirectory + "/../target/universal"

let package = Package(
    name: "KVM",
    platforms: [.macOS(.v13)],
    targets: [
        .systemLibrary(name: "CKvm", path: "Sources/CKvm"),
        .executableTarget(
            name: "KVM",
            dependencies: ["CKvm"],
            linkerSettings: [
                .unsafeFlags(["-L", rustLib]),
                .linkedFramework("Security"),
                .linkedFramework("SystemConfiguration"),
                .linkedFramework("CoreWLAN"),
                .linkedFramework("IOKit"),
            ]
        ),
    ]
)
