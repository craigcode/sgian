// swift-tools-version: 6.0

import PackageDescription

let package = Package(
    name: "SgianMac",
    platforms: [
        .macOS(.v13),
    ],
    products: [
        .executable(name: "SgianMac", targets: ["SgianMac"]),
    ],
    dependencies: [
        .package(url: "https://github.com/migueldeicaza/SwiftTerm.git", exact: "1.19.0"),
        .package(url: "https://github.com/sparkle-project/Sparkle", exact: "2.9.6"),
    ],
    targets: [
        .executableTarget(
            name: "SgianMac",
            dependencies: ["SwiftTerm", .product(name: "Sparkle", package: "Sparkle")],
            path: "Sources/SgianMac",
            swiftSettings: [.swiftLanguageMode(.v5)],
            linkerSettings: [.unsafeFlags(["-Xlinker", "-rpath", "-Xlinker", "@executable_path/../Frameworks"])]
        ),
        .testTarget(
            name: "SgianMacTests",
            dependencies: ["SgianMac"],
            path: "Tests/SgianMacTests",
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
    ]
)
