import Foundation
import Security

/// Per-workspace client credentials (docs/design/client-identity.md) in the
/// login Keychain: one generic-password item per workspace path. The
/// environment (`SGIAN_CLIENT_TOKEN` / `SGIAN_CLIENT_TOKEN_FILE`) always
/// wins over the Keychain so the SSH path keeps working unchanged.
enum CredentialStore {
    private static let service = "dev.sgian.client-credential"

    private static func query(forAccount account: String) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
    }

    private static func query(for workspace: URL) -> [String: Any] {
        query(forAccount: workspace.standardizedFileURL.path)
    }

    private static func loadAccount(_ account: String) -> String? {
        var query = query(forAccount: account)
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

    @discardableResult
    private static func saveAccount(_ token: String?, account: String) -> Bool {
        let base = query(forAccount: account)
        SecItemDelete(base as CFDictionary)
        guard let token = token?.trimmingCharacters(in: .whitespacesAndNewlines), !token.isEmpty else {
            return true
        }
        var add = base
        add[kSecValueData as String] = Data(token.utf8)
        add[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlockedThisDeviceOnly
        return SecItemAdd(add as CFDictionary, nil) == errSecSuccess
    }

    static func load(for workspace: URL) -> String? {
        loadAccount(workspace.standardizedFileURL.path)
    }

    /// Store (replacing any previous item) or, with nil, remove.
    @discardableResult
    static func save(_ token: String?, for workspace: URL) -> Bool {
        saveAccount(token, account: workspace.standardizedFileURL.path)
    }

    /// Resolve a saved machine's credential from the login Keychain
    /// (`machine:<name>`), falling back to the private 0600 token file written
    /// by `sgian ctl machine add --token-stdin`.
    static func loadMachine(_ name: String, fallbackTokenURL: URL? = nil) -> String? {
        if let keychainToken = loadAccount("machine:\(name)") {
            return keychainToken
        }
        guard let fallbackTokenURL,
              let contents = try? String(contentsOf: fallbackTokenURL, encoding: .utf8)
        else { return nil }
        let first = contents
            .split(whereSeparator: \.isNewline)
            .first
            .map { $0.trimmingCharacters(in: .whitespaces) } ?? ""
        return first.isEmpty ? nil : first
    }

    @discardableResult
    static func saveMachine(_ token: String?, for name: String) -> Bool {
        saveAccount(token, account: "machine:\(name)")
    }
}
