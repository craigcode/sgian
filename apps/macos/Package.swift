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
    ],
    targets: [
        .executableTarget(
            name: "SgianMac",
            dependencies: ["SwiftTerm"],
            path: "Sources/SgianMac",
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
        .testTarget(
            name: "SgianMacTests",
            dependencies: ["SgianMac"],
            path: "Tests/SgianMacTests",
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
    ]
)
