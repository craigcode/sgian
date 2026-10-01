import Foundation
import Security

/// Per-workspace client credentials (docs/design/client-identity.md) in the
/// login Keychain: one generic-password item per workspace path. The
/// environment (`SGIAN_CLIENT_TOKEN` / `SGIAN_CLIENT_TOKEN_FILE`) always
/// wins over the Keychain so the SSH path keeps working unchanged.
enum CredentialStore {
    private static let service = "dev.sgian.client-credential"

    private static func query(for workspace: URL) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: workspace.standardizedFileURL.path,
        ]
    }

    static func load(for workspace: URL) -> String? {
        var query = query(for: workspace)
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var item: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess,
              let data = item as? Data,
              let token = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines),
              !token.isEmpty
        else { return nil }
        return token
    }

    /// Store (replacing any previous item) or, with nil, remove.
    @discardableResult
    static func save(_ token: String?, for workspace: URL) -> Bool {
        let base = query(for: workspace)
        SecItemDelete(base as CFDictionary)
        guard let token = token?.trimmingCharacters(in: .whitespacesAndNewlines), !token.isEmpty else {
            return true
        }
        var add = base
        add[kSecValueData as String] = Data(token.utf8)
        add[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlockedThisDeviceOnly
        return SecItemAdd(add as CFDictionary, nil) == errSecSuccess
    }
}
