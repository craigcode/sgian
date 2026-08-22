import Darwin
import Foundation

struct WorkspaceLocator: Equatable, Sendable {
    let workspaceURL: URL
    let appSupportURL: URL
    let workspaceKey: String

    init(workspaceURL: URL, fileManager: FileManager = .default) {
        let standardized = Self.canonicalWorkspaceURL(workspaceURL)
        self.workspaceURL = standardized

        let dataRoot = fileManager.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        let current = dataRoot.appendingPathComponent("Sgian", isDirectory: true)
        let legacy = dataRoot.appendingPathComponent("Sgian2", isDirectory: true)
        if fileManager.fileExists(atPath: current.path) || !fileManager.fileExists(atPath: legacy.path) {
            appSupportURL = current
        } else {
            appSupportURL = legacy
        }
        workspaceKey = Self.fnvWorkspaceKey(standardized.path)
    }

    var dataDirectoryURL: URL {
        Self.resolveDataDirectory(
            workspacesRoot: appSupportURL.appendingPathComponent("workspaces", isDirectory: true),
            workspaceURL: workspaceURL,
            workspaceKey: workspaceKey
        )
    }

    var runtimeDirectoryURL: URL {
        appSupportURL
            .appendingPathComponent("runtime", isDirectory: true)
            .appendingPathComponent(workspaceKey, isDirectory: true)
    }

    var socketURL: URL { runtimeDirectoryURL.appendingPathComponent("daemon.sock") }
    var tokenURL: URL { dataDirectoryURL.appendingPathComponent("daemon.token") }

    static func fnvWorkspaceKey(_ path: String) -> String {
        var hash: UInt64 = 0xcbf29ce484222325
        for byte in path.utf8 {
            hash ^= UInt64(byte)
            hash = hash &* 0x100000001b3
        }
        return String(format: "%016llx", hash)
    }

    /// Current Rust builds migrate legacy DefaultHasher-keyed directories to
    /// the stable FNV key. If that rename could not happen, locate the legacy
    /// state by its persisted canonical cwd rather than reimplementing an
    /// intentionally unstable Rust hasher in Swift.
    static func resolveDataDirectory(
        workspacesRoot: URL,
        workspaceURL: URL,
        workspaceKey: String,
        fileManager: FileManager = .default
    ) -> URL {
        let current = workspacesRoot.appendingPathComponent(workspaceKey, isDirectory: true)
        if fileManager.fileExists(atPath: current.path) { return current }

        let targetPath = canonicalWorkspaceURL(workspaceURL).path
        guard let candidates = try? fileManager.contentsOfDirectory(
            at: workspacesRoot,
            includingPropertiesForKeys: [.isDirectoryKey],
            options: [.skipsHiddenFiles]
        ) else {
            return current
        }
        for candidate in candidates where candidate != current {
            let values = try? candidate.resourceValues(forKeys: [.isDirectoryKey])
            guard values?.isDirectory == true else { continue }
            let stateURL = candidate.appendingPathComponent("workspace.json")
            guard let data = try? Data(contentsOf: stateURL),
                  let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                  let cwd = object["cwd"] as? String,
                  canonicalWorkspaceURL(URL(fileURLWithPath: cwd, isDirectory: true)).path == targetPath
            else { continue }
            return candidate
        }
        return current
    }

    static func canonicalWorkspaceURL(_ url: URL) -> URL {
        let path = url.standardizedFileURL.path
        var buffer = [CChar](repeating: 0, count: Int(PATH_MAX))
        if Darwin.realpath(path, &buffer) != nil {
            return URL(fileURLWithPath: String(cString: buffer), isDirectory: true)
        }
        return URL(fileURLWithPath: path, isDirectory: true)
    }
}
