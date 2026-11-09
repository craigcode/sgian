import Darwin
import Foundation

struct SavedMachine: Codable, Equatable, Identifiable, Sendable {
    let name: String
    let transport: String
    let host: String?
    let port: UInt16?
    let workspace: String
    let target: String?
    let tokenFile: String?

    var id: String { name }

    var displaySubtitle: String {
        if let host, !host.isEmpty {
            let endpoint = port.map { "\(host):\($0)" } ?? host
            return "\(endpoint) · \(workspace)"
        }
        if let target, !target.isEmpty {
            return "\(target) · \(workspace)"
        }
        return workspace
    }

    enum CodingKeys: String, CodingKey {
        case name
        case transport
        case host
        case port
        case workspace
        case target
        case tokenFile = "token_file"
    }

    init(
        name: String,
        transport: String = "ssh",
        host: String? = nil,
        port: UInt16? = nil,
        workspace: String,
        target: String? = nil,
        tokenFile: String? = nil
    ) {
        self.name = name
        self.transport = transport
        self.host = host
        self.port = port
        self.workspace = workspace
        self.target = target
        self.tokenFile = tokenFile
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        name = try container.decode(String.self, forKey: .name)
        transport = try container.decodeIfPresent(String.self, forKey: .transport) ?? "ssh"
        host = try container.decodeIfPresent(String.self, forKey: .host)
        port = try container.decodeIfPresent(UInt16.self, forKey: .port)
        workspace = try container.decode(String.self, forKey: .workspace)
        target = try container.decodeIfPresent(String.self, forKey: .target)
        tokenFile = try container.decodeIfPresent(String.self, forKey: .tokenFile)
    }
}

private struct GlobalMachinesConfig: Decodable {
    var machines: [SavedMachine]?
}

struct WorkspaceLocator: Equatable, Sendable {
    let workspaceURL: URL
    let appSupportURL: URL
    let workspaceKey: String
    let machine: SavedMachine?

    init(workspaceURL: URL, fileManager: FileManager = .default) {
        machine = nil
        let standardized = Self.canonicalWorkspaceURL(workspaceURL)
        self.workspaceURL = standardized
        appSupportURL = Self.resolveAppSupportURL(fileManager: fileManager)
        workspaceKey = Self.fnvWorkspaceKey(standardized.path)
    }

    init(machine: SavedMachine, appSupportURL: URL? = nil, fileManager: FileManager = .default) {
        self.machine = machine
        self.appSupportURL = appSupportURL ?? Self.resolveAppSupportURL(fileManager: fileManager)
        workspaceURL = URL(fileURLWithPath: machine.workspace, isDirectory: true)
        workspaceKey = "machine:\(machine.name)"
    }

    static func resolveAppSupportURL(fileManager: FileManager = .default) -> URL {
        if let override = ProcessInfo.processInfo.environment["SGIAN_MACHINES_HOME"]?
            .trimmingCharacters(in: .whitespacesAndNewlines),
           !override.isEmpty
        {
            return URL(fileURLWithPath: override, isDirectory: true)
        }
        let dataRoot = fileManager.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        let current = dataRoot.appendingPathComponent("Sgian", isDirectory: true)
        let legacy = dataRoot.appendingPathComponent("Sgian2", isDirectory: true)
        if fileManager.fileExists(atPath: current.path) || !fileManager.fileExists(atPath: legacy.path) {
            return current
        }
        return legacy
    }

    static func loadSavedMachines(
        appSupportURL: URL? = nil,
        fileManager: FileManager = .default
    ) -> [SavedMachine] {
        let root = appSupportURL ?? resolveAppSupportURL(fileManager: fileManager)
        let configURL = root.appendingPathComponent("config.json")
        guard let data = try? Data(contentsOf: configURL),
              let decoded = try? JSONDecoder().decode(GlobalMachinesConfig.self, from: data)
        else {
            return []
        }
        return decoded.machines ?? []
    }

    var dataDirectoryURL: URL {
        Self.resolveDataDirectory(
            workspacesRoot: appSupportURL.appendingPathComponent("workspaces", isDirectory: true),
            workspaceURL: workspaceURL,
            workspaceKey: workspaceKey
        )
    }

    var runtimeDirectoryURL: URL {
        if let machine {
            return appSupportURL
                .appendingPathComponent("runtime", isDirectory: true)
                .appendingPathComponent("machines", isDirectory: true)
                .appendingPathComponent(machine.name, isDirectory: true)
        }
        return appSupportURL
            .appendingPathComponent("runtime", isDirectory: true)
            .appendingPathComponent(workspaceKey, isDirectory: true)
    }

    var socketURL: URL { runtimeDirectoryURL.appendingPathComponent("daemon.sock") }

    var tokenURL: URL {
        if let machine {
            if let tokenFile = machine.tokenFile?.trimmingCharacters(in: .whitespacesAndNewlines),
               !tokenFile.isEmpty
            {
                return URL(fileURLWithPath: tokenFile)
            }
            return appSupportURL
                .appendingPathComponent("machines", isDirectory: true)
                .appendingPathComponent("\(machine.name).token")
        }
        return dataDirectoryURL.appendingPathComponent("daemon.token")
    }

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
