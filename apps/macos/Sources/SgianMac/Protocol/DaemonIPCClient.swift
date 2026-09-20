import Foundation

enum DaemonClientError: LocalizedError {
    case tokenUnavailable(URL)
    case authentication(String)
    case request(String)
    case invalidResponse
    case backendUnavailable
    case backendLaunch(String)

    var errorDescription: String? {
        switch self {
        case let .tokenUnavailable(url): "Daemon token is unavailable at \(url.path)"
        case let .authentication(message): "Daemon authentication failed: \(message)"
        case let .request(message): message
        case .invalidResponse: "The daemon returned an invalid response"
        case .backendUnavailable:
            "The Sgian backend could not be found. Set SGIAN_BACKEND_BINARY or build the native app bundle."
        case let .backendLaunch(message): "Could not start the Sgian backend: \(message)"
        }
    }
}

final class DaemonIPCClient: @unchecked Sendable {
    private let locator: WorkspaceLocator
    /// Terminal key sequences must reach the daemon in exactly the order the
    /// view produced them. A serial queue also orders resize/focus relative to
    /// input, mirroring the existing Tauri terminal controller.
    private let requestQueue = DispatchQueue(label: "dev.sgian.native.ipc.requests", qos: .userInitiated)
    /// A subscription blocks in readLine for the life of the app, so it cannot
    /// share the serial request queue.
    private let subscriptionQueue = DispatchQueue(label: "dev.sgian.native.ipc.subscription", qos: .userInitiated)
    private let fileManager: FileManager

    init(locator: WorkspaceLocator, fileManager: FileManager = .default) {
        self.locator = locator
        self.fileManager = fileManager
    }

    func connectOrLaunch() async throws {
        if (try? await ping()) == true { return }
        try await launchBackend()
        var lastError: Error = DaemonClientError.invalidResponse
        for attempt in 0..<50 {
            do {
                if try await ping() { return }
            } catch {
                lastError = error
            }
            try await Task.sleep(for: .milliseconds(attempt < 10 ? 50 : 100))
        }
        throw lastError
    }

    func ping() async throws -> Bool {
        let response: CommandOK = try await request(["command": .string("ping")], as: CommandOK.self)
        return response.ok
    }

    func request<T: Decodable & Sendable>(_ fields: [String: JSONValue], as type: T.Type) async throws -> T {
        try await withCheckedThrowingContinuation { continuation in
            requestQueue.async { [locator] in
                do {
                    let result = try Self.performRequest(locator: locator, fields: fields, as: type)
                    continuation.resume(returning: result)
                } catch {
                    continuation.resume(throwing: error)
                }
            }
        }
    }

    func subscribe(
        onReady: @escaping @Sendable () -> Void,
        onEvent: @escaping @Sendable (DaemonEvent) -> Void,
        onDisconnect: @escaping @Sendable (Error) -> Void
    ) -> Subscription {
        let subscription = Subscription()
        subscriptionQueue.async { [locator] in
            do {
                let authenticated = try Self.authenticatedSocket(locator: locator)
                let socket = authenticated.socket
                subscription.install(socket: socket)
                let request = JSONValue.object(["command": .string("subscribe")])
                try socket.writeLine(JSONEncoder.ipc.encode(request))
                if authenticated.supportsSubscribeAck {
                    let firstEvent = try JSONDecoder.ipc.decode(DaemonEvent.self, from: socket.readLine())
                    guard firstEvent.kind == "subscribe_ack" else {
                        throw DaemonClientError.request(
                            "Expected subscription acknowledgement, received \(firstEvent.kind)"
                        )
                    }
                }
                try socket.setReadTimeout(seconds: nil)
                onReady()
                while !subscription.isCancelled {
                    let line = try socket.readLine()
                    let event = try JSONDecoder.ipc.decode(DaemonEvent.self, from: line)
                    onEvent(event)
                }
            } catch {
                if !subscription.isCancelled { onDisconnect(error) }
            }
        }
        return subscription
    }

    private static func performRequest<T: Decodable & Sendable>(
        locator: WorkspaceLocator,
        fields: [String: JSONValue],
        as type: T.Type
    ) throws -> T {
        let socket = try authenticatedSocket(locator: locator).socket
        defer { socket.close() }
        try socket.writeLine(JSONEncoder.ipc.encode(JSONValue.object(fields)))
        let response = try JSONDecoder.ipc.decode(IPCResponse.self, from: socket.readLine())
        guard response.ok else {
            throw DaemonClientError.request(response.error ?? "Daemon request failed")
        }
        let resultData = try JSONEncoder.ipc.encode(response.result)
        return try JSONDecoder.ipc.decode(type, from: resultData)
    }

    /// (M6) The per-client credential this process presents, if any:
    /// `SGIAN_CLIENT_TOKEN`, else the first line of `SGIAN_CLIENT_TOKEN_FILE`.
    static func clientTokenFromEnvironment() -> String? {
        let environment = ProcessInfo.processInfo.environment
        if let token = environment["SGIAN_CLIENT_TOKEN"]?.trimmingCharacters(in: .whitespacesAndNewlines), !token.isEmpty {
            return token
        }
        guard let path = environment["SGIAN_CLIENT_TOKEN_FILE"],
              let contents = try? String(contentsOfFile: path, encoding: .utf8)
        else { return nil }
        let first = contents.split(whereSeparator: \.isNewline).first.map { $0.trimmingCharacters(in: .whitespaces) } ?? ""
        return first.isEmpty ? nil : first
    }

    /// The credential this client presents for a workspace: the environment
    /// first, then the login Keychain (`CredentialStore`).
    static func clientToken(for workspace: URL) -> String? {
        clientTokenFromEnvironment() ?? CredentialStore.load(for: workspace)
    }

    private static func authenticatedSocket(locator: WorkspaceLocator) throws -> AuthenticatedSocket {
        let clientToken = clientToken(for: locator.workspaceURL)
        let fileToken = (try? String(contentsOf: locator.tokenURL, encoding: .utf8))?
            .trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        // A remote client has no workspace token file; its credential rides
        // the hello instead (docs/design/client-identity.md).
        guard !fileToken.isEmpty || clientToken != nil
        else { throw DaemonClientError.tokenUnavailable(locator.tokenURL) }

        let socket = try UnixSocket(path: locator.socketURL.path)
        var fields: [String: JSONValue] = [
            "type": .string("hello"),
            "version": .number(1),
            "token": .string(fileToken),
            "capabilities": .array([.string("subscribe-ack")]),
        ]
        if let clientToken { fields["client_token"] = .string(clientToken) }
        let hello = JSONValue.object(fields)
        try socket.writeLine(JSONEncoder.ipc.encode(hello))
        let response = try JSONDecoder.ipc.decode(IPCResponse.self, from: socket.readLine())
        guard response.ok else {
            socket.close()
            throw DaemonClientError.authentication(response.error ?? "rejected")
        }
        let capabilities = response.result["capabilities"]?.arrayValue?.compactMap(\.stringValue) ?? []
        return AuthenticatedSocket(
            socket: socket,
            supportsSubscribeAck: capabilities.contains("subscribe-ack")
        )
    }

    private func launchBackend() async throws {
        let executable = try locateBackend()
        try await withCheckedThrowingContinuation { continuation in
            requestQueue.async { [locator] in
                let process = Process()
                process.executableURL = executable
                process.arguments = ["ctl", "--workspace", locator.workspaceURL.path, "sync", "off"]
                let diagnostics = Pipe()
                process.standardOutput = diagnostics
                process.standardError = diagnostics
                do {
                    try process.run()
                    process.waitUntilExit()
                    if process.terminationStatus == 0 {
                        continuation.resume()
                    } else {
                        let data = diagnostics.fileHandleForReading.readDataToEndOfFile()
                        let message = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines)
                        continuation.resume(throwing: DaemonClientError.backendLaunch(message ?? "exit \(process.terminationStatus)"))
                    }
                } catch {
                    continuation.resume(throwing: error)
                }
            }
        }
    }

    private func locateBackend() throws -> URL {
        let environment = ProcessInfo.processInfo.environment
        var candidates: [URL] = []
        if let configured = environment["SGIAN_BACKEND_BINARY"], !configured.isEmpty {
            candidates.append(URL(fileURLWithPath: configured))
        }
        candidates.append(Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/sgian"))

        let cwd = URL(fileURLWithPath: fileManager.currentDirectoryPath, isDirectory: true)
        candidates.append(cwd.appendingPathComponent("src-tauri/target/debug/sgian"))
        candidates.append(cwd.appendingPathComponent("src-tauri/target/release/sgian"))
        candidates.append(cwd.appendingPathComponent("../../src-tauri/target/debug/sgian").standardizedFileURL)
        candidates.append(cwd.appendingPathComponent("../../src-tauri/target/release/sgian").standardizedFileURL)

        for directory in (environment["PATH"] ?? "").split(separator: ":") {
            candidates.append(URL(fileURLWithPath: String(directory)).appendingPathComponent("sgian"))
        }
        guard let match = candidates.first(where: { fileManager.isExecutableFile(atPath: $0.path) }) else {
            throw DaemonClientError.backendUnavailable
        }
        return match
    }
}

private struct AuthenticatedSocket {
    let socket: UnixSocket
    let supportsSubscribeAck: Bool
}

final class Subscription: @unchecked Sendable {
    private let lock = NSLock()
    private var socket: UnixSocket?
    private var cancelled = false

    var isCancelled: Bool { lock.withLock { cancelled } }

    func install(socket: UnixSocket) {
        lock.withLock {
            if cancelled { socket.close() } else { self.socket = socket }
        }
    }

    func cancel() {
        lock.withLock {
            cancelled = true
            socket?.close()
            socket = nil
        }
    }

    deinit { cancel() }
}
