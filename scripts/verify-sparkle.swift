import CryptoKit
import Foundation

// Independently verify against the key embedded in the app, not the signing key.
guard CommandLine.arguments.count == 4,
      let key = Data(base64Encoded: CommandLine.arguments[1]), key.count == 32,
      let signature = Data(base64Encoded: CommandLine.arguments[2]), signature.count == 64
else { fputs("Expected public key, signature, and archive path\n", stderr); exit(1) }
let publicKey = try Curve25519.Signing.PublicKey(rawRepresentation: key)
let archive = try Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[3]), options: .mappedIfSafe)
guard publicKey.isValidSignature(signature, for: archive) else {
    fputs("Sparkle archive does not match the app’s embedded public key\n", stderr); exit(1)
}
print("Sparkle archive signature verified against embedded public key")
