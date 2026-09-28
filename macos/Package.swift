// swift-tools-version:5.10
//
// SwiftPM manifest for the Thurm macOS front end.
//
// The Rust core is linked as a static library (`libthurm_ffi.a`). `build.sh` builds it
// (arm64) into `../target/aarch64-apple-darwin/release` and copies the C header into
// `Sources/CThurm/include/`. Override the library directory with `THURM_LIB_DIR`.

import PackageDescription

let libDir = Context.environment["THURM_LIB_DIR"]
    ?? (Context.packageDirectory + "/../target/aarch64-apple-darwin/release")

// SwiftPM's link stamps the deployment target as the SDK version (LC_BUILD_VERSION sdk 14.0),
// which makes AppKit run the app in its pre-Liquid Glass compatibility look. build.sh passes the
// real SDK version (`xcrun --show-sdk-version`) so the binary is marked as built for it.
let sdkVersionFlags: [String] = Context.environment["THURM_SDK_VERSION"].map {
    ["-Xlinker", "-platform_version", "-Xlinker", "macos", "-Xlinker", "14.0", "-Xlinker", $0]
} ?? []

let package = Package(
    name: "Thurm",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "Thurm", targets: ["Thurm"]),
        .executable(name: "thurm-intelligence", targets: ["ThurmIntelligence"]),
    ],
    targets: [
        // C module exposing thurm.h (see Sources/CThurm/module.modulemap).
        .systemLibrary(name: "CThurm", path: "Sources/CThurm"),
        // Automatic updates. build.sh embeds the framework in Contents/Frameworks. The archive
        // also carries Sparkle's tools (sign_update), used by macos/release.
        .binaryTarget(
            name: "Sparkle",
            url: "https://github.com/sparkle-project/Sparkle/releases/download/2.10.0/Sparkle-for-Swift-Package-Manager.zip",
            checksum: "17e28312b8e18ab7cdbbe09a6fb28cc55a5479ec6c371dbc07cdecd2a14fd959"
        ),
        .executableTarget(
            name: "Thurm",
            dependencies: ["CThurm", "Sparkle"],
            path: "Sources/Thurm",
            linkerSettings: [
                .unsafeFlags(["-L", libDir, "-Xlinker", "-rpath", "-Xlinker", "@executable_path/../Frameworks"]
                    + sdkVersionFlags),
                .linkedLibrary("thurm_ffi"),
                .linkedFramework("AppKit"),
                .linkedFramework("Metal"),
                .linkedFramework("QuartzCore"),
                .linkedFramework("CoreText"),
                .linkedFramework("CoreGraphics"),
                .linkedFramework("Carbon"),
                .linkedFramework("UserNotifications"),
                // Needed by Rust's std / dependencies inside the static library.
                .linkedFramework("CoreFoundation"),
                .linkedFramework("Security"),
                .linkedFramework("SystemConfiguration"),
                .linkedLibrary("iconv"),
                .linkedLibrary("resolv"),
            ]
        ),
        // Runs prompts on Apple Intelligence's on-device model for thurmd (`[ai]` in the
        // config). Weakly linked: the framework only exists on macOS 26 and later.
        .executableTarget(
            name: "ThurmIntelligence",
            path: "Sources/ThurmIntelligence",
            linkerSettings: [
                .unsafeFlags(sdkVersionFlags + ["-Xlinker", "-weak_framework", "-Xlinker", "FoundationModels"]),
            ]
        ),
    ]
)
