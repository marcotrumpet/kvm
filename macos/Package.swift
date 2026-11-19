// swift-tools-version:5.10
import PackageDescription

// The Rust core is linked as a static library built by build.sh into ../target/universal.
let rustLib = Context.packageDirectory + "/../target/universal"

let package = Package(
    name: "KVM",
    platforms: [.macOS(.v13)],
    dependencies: [
        // Updates (see release.sh).
        .package(url: "https://github.com/sparkle-project/Sparkle", exact: "2.10.1"),
    ],
    targets: [
        .systemLibrary(name: "CKvm", path: "Sources/CKvm"),
        .target(name: "CGVirtualDisplay", path: "Sources/CGVirtualDisplay"),
        .executableTarget(
            name: "KVM",
            dependencies: ["CKvm", "CGVirtualDisplay", .product(name: "Sparkle", package: "Sparkle")],
            linkerSettings: [
                // build.sh puts Sparkle.framework in Contents/Frameworks.
                .unsafeFlags(["-L", rustLib, "-Xlinker", "-rpath", "-Xlinker", "@executable_path/../Frameworks"]),
                .linkedFramework("CoreGraphics"),
                .linkedFramework("Security"),
                .linkedFramework("SystemConfiguration"),
                .linkedFramework("CoreWLAN"),
                .linkedFramework("IOKit"),
            ]
        ),
    ]
)
