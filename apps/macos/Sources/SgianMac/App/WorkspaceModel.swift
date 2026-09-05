import AppKit
import Combine
import Foundation

@MainActor
final class WorkspaceModel: ObservableObject {
    @Published private(set) var panes: [Pane] = []
    @Published var selectedPaneID: String?
    @Published private(set) var paneStates: [String: PaneRuntimeState] = [:]
    @Published private(set) var agentStates: [String: AgentPaneInfo] = [:]
    @Published private(set) var agentSpecs: [String: AgentPaneSpec] = [:]
    @Published private(set) var chats: [String: AgentChatState] = [:]
    @Published private(set) var terminals: [String: TerminalSurface] = [:]
    @Published private(set) var status: ConnectionStatus = .disconnected
    @Published var errorMessage: String?
    @Published private(set) var workspaceURL: URL
    @Published private(set) var layout: PaneLayout?
    @Published var zoomed = false
    @Published var showingCommands = false
    @Published var showingSearch = false
    @Published var panePendingClose: Pane?
    @Published private(set) var recentWorkspaces: [String] = UserDefaults.standard.stringArray(forKey: "recentWorkspaces") ?? []
    @Published var terminalFontSize: Double {
        didSet {
            UserDefaults.standard.set(terminalFontSize, forKey: "terminalFontSize")
            terminals.values.forEach { $0.setFontSize(terminalFontSize) }
        }
    }

    private var client: DaemonIPCClient?
    private var subscription: Subscription?
    private var connectionTask: Task<Void, Never>?
    private var reconnectTask: Task<Void, Never>?
    private var reconnectAttempt = 0
    private var generation = UUID()
    private var pendingInput: [String: String] = [:]
    private var inputDrains: [String: Task<Void, Never>] = [:]
    private var ensuringTerminals: Set<String> = []
    private var layoutSaveTask: Task<Void, Never>?

    init() {
        let defaults = UserDefaults.standard
        let environmentPath = ProcessInfo.processInfo.environment["SGIAN_WORKSPACE"]
        let savedPath = defaults.string(forKey: "workspacePath")
        let current = FileManager.default.currentDirectoryPath
        let fallback = current == "/" ? FileManager.default.homeDirectoryForCurrentUser.path : current
        workspaceURL = URL(fileURLWithPath: environmentPath ?? savedPath ?? fallback, isDirectory: true)
        let savedFont = defaults.double(forKey: "terminalFontSize")
        terminalFontSize = savedFont == 0 ? 13 : savedFont
    }

    deinit {
        connectionTask?.cancel()
        reconnectTask?.cancel()
        subscription?.cancel()
        inputDrains.values.forEach { $0.cancel() }
    }

    var selectedPane: Pane? {
        panes.first { $0.id == selectedPaneID }
    }

    func start() {
        guard connectionTask == nil else { return }
        connect(to: workspaceURL)
    }

    func connect(to url: URL) {
        let nextURL = url.standardizedFileURL.resolvingSymlinksInPath()
        var isDirectory: ObjCBool = false
        guard FileManager.default.fileExists(atPath: nextURL.path, isDirectory: &isDirectory), isDirectory.boolValue else {
            errorMessage = "Workspace does not exist: \(nextURL.path)"
            return
        }
        connectionTask?.cancel()
        layoutSaveTask?.cancel()
        reconnectTask?.cancel()
        reconnectTask = nil
        reconnectAttempt = 0
        subscription?.cancel()
        subscription = nil
        client = nil
        inputDrains.values.forEach { $0.cancel() }
        inputDrains = [:]
        pendingInput = [:]
        ensuringTerminals = []
        generation = UUID()
        let currentGeneration = generation
        workspaceURL = nextURL
        UserDefaults.standard.set(nextURL.path, forKey: "workspacePath")
        recentWorkspaces = Array(([nextURL.path] + recentWorkspaces.filter { $0 != nextURL.path }).prefix(10))
        UserDefaults.standard.set(recentWorkspaces, forKey: "recentWorkspaces")
        status = .connecting
        errorMessage = nil
        panes = []
        selectedPaneID = nil
        paneStates = [:]
        agentStates = [:]
        agentSpecs = [:]
        terminals = [:]
        chats = [:]
        layout = nil
        zoomed = false
        panePendingClose = nil

        connectionTask = Task { [weak self] in
            guard let self else { return }
            do {
                let client = DaemonIPCClient(locator: WorkspaceLocator(workspaceURL: nextURL))
                try await client.connectOrLaunch()
                guard currentGeneration == generation else { return }
                self.client = client
                let snapshot: WorkspaceSnapshot = try await client.request(
                    ["command": .string("bootstrap_workspace")],
                    as: WorkspaceSnapshot.self
                )
                guard currentGeneration == generation else { return }
                apply(snapshot)
                status = .connected
                beginSubscription(client: client, generation: currentGeneration)
                try await completeUISmokeIfRequested()
            } catch {
                guard currentGeneration == generation else { return }
                status = .failed(error.localizedDescription)
                errorMessage = error.localizedDescription
                failUISmokeIfRequested(error)
            }
        }
    }

    func chooseWorkspace() {
        let panel = NSOpenPanel()
        panel.title = "Choose a Sgian workspace"
        panel.prompt = "Open Workspace"
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.directoryURL = workspaceURL
        guard panel.runModal() == .OK, let url = panel.url else { return }
        connect(to: url)
    }

    func select(_ paneID: String?) {
        selectedPaneID = paneID
        guard let paneID else { return }
        Task { [weak self] in
            guard let self, let client else { return }
            do {
                let _: CommandOK = try await client.request([
                    "command": .string("set_active_pane"),
                    "pane_id": .string(paneID),
                ], as: CommandOK.self)
                terminals[paneID]?.focus()
            } catch { present(error) }
        }
    }

    func createShell(title: String? = nil, direction: String = "row", profile: String? = nil) {
        let anchor = selectedPaneID
        perform { client in
            let pane: Pane = try await client.request([
                "command": .string("create_pane"),
                "title": title.map(JSONValue.string) ?? .null,
                "profile": profile.map(JSONValue.string) ?? .null,
            ], as: Pane.self)
            guard self.client === client else { return }
            self.upsert(pane)
            self.layout = self.layout?.inserting(pane.id, beside: anchor, direction: direction) ?? .leaf(pane.id)
            self.zoomed = false
            self.saveLayout()
            self.select(pane.id)
        }
    }

    func createAgent(backend: AgentBackend, model: String? = nil) {
        perform { client in
            let pane: Pane = try await client.request([
                "command": .string("create_agent_pane_with_spec"),
                "title": .null,
                "backend": .string(backend.rawValue),
                "model": model.map(JSONValue.string) ?? .null,
            ], as: Pane.self)
            guard self.client === client else { return }
            self.agentSpecs[pane.id] = AgentPaneSpec(backend: backend, model: model)
            self.upsert(pane)
            self.saveLayout()
            self.select(pane.id)
        }
    }

    func close(_ paneID: String) {
        perform { client in
            let snapshot: WorkspaceSnapshot = try await client.request([
                "command": .string("close_pane"),
                "pane_id": .string(paneID),
            ], as: WorkspaceSnapshot.self)
            guard self.client === client else { return }
            self.apply(snapshot)
            self.saveLayout()
        }
    }

    func rename(_ paneID: String, to title: String) {
        let trimmed = title.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }
        perform { client in
            let pane: Pane = try await client.request([
                "command": .string("rename_pane"),
                "pane_id": .string(paneID),
                "title": .string(trimmed),
            ], as: Pane.self)
            guard self.client === client else { return }
            self.upsert(pane)
        }
    }

    func restart(_ paneID: String) {
        perform { client in
            let _: CommandOK = try await client.request([
                "command": .string("restart_pane_terminal"),
                "pane_id": .string(paneID),
            ], as: CommandOK.self)
            guard self.client === client else { return }
            self.paneStates[paneID] = .live
            if self.panes.first(where: { $0.id == paneID })?.kind == .shell {
                self.terminals[paneID]?.feed("\u{1b}[2J\u{1b}[H")
            }
        }
    }

    func sendAgentMessage(paneID: String, text: String) {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, chats[paneID]?.busy != true else { return }
        var chat = chats[paneID] ?? AgentChatState()
        chat.appendUserMessage(trimmed)
        chats[paneID] = chat
        perform { client in
            do {
                let _: CommandOK = try await client.request([
                    "command": .string("send_agent_message"),
                    "pane_id": .string(paneID),
                    "text": .string(trimmed),
                ], as: CommandOK.self)
            } catch {
                guard self.client === client else { return }
                var failed = self.chats[paneID] ?? AgentChatState()
                failed.removeLastUserMessage(matching: trimmed)
                self.chats[paneID] = failed
                throw error
            }
        }
    }

    func interruptAgent(_ paneID: String) {
        perform { client in
            let _: CommandOK = try await client.request([
                "command": .string("interrupt_agent"),
                "pane_id": .string(paneID),
            ], as: CommandOK.self)
        }
    }

    func answerPermission(paneID: String, requestID: String, allow: Bool, message: String? = nil) {
        perform { client in
            let _: CommandOK = try await client.request([
                "command": .string("agent_approval"),
                "pane_id": .string(paneID),
                "request_id": .string(requestID),
                "allow": .bool(allow),
                "message": message.map(JSONValue.string) ?? .null,
            ], as: CommandOK.self)
            guard self.client === client else { return }
            var chat = self.chats[paneID] ?? AgentChatState()
            if chat.pendingPermission?.requestID == requestID { chat.pendingPermission = nil }
            self.chats[paneID] = chat
        }
    }

    func clearSelectedTerminal() {
        guard let selectedPaneID else { return }
        terminals[selectedPaneID]?.clearScrollback()
    }

    func resizeSplit(_ id: String, ratio: Double) {
        layout = layout?.resizing(id, ratio: ratio)
        saveLayout()
    }

    func toggleZoom() { zoomed.toggle() }

    func focusNext(_ offset: Int) {
        let ids = layout?.paneIDs ?? panes.map(\.id)
        guard !ids.isEmpty else { return }
        let index = ids.firstIndex(of: selectedPaneID ?? "") ?? 0
        select(ids[(index + offset + ids.count) % ids.count])
    }

    func requestClose(_ pane: Pane? = nil) { panePendingClose = pane ?? selectedPane }

    private func saveLayout() {
        layoutSaveTask?.cancel()
        guard let client else { return }
        let value = layout?.json ?? .null
        layoutSaveTask = Task { [weak self] in
            do {
                try await Task.sleep(for: .milliseconds(200))
                guard let self, self.client === client, !Task.isCancelled else { return }
                let _: CommandOK = try await client.request([
                    "command": .string("update_workspace_layout"), "layout": value,
                ], as: CommandOK.self)
            } catch is CancellationError {} catch {
                guard let self, self.client === client else { return }
                self.present(error)
            }
        }
    }

    private func beginSubscription(client: DaemonIPCClient, generation: UUID) {
        subscription?.cancel()
        subscription = client.subscribe(
            onReady: { [weak self] in
                Task { @MainActor in
                    guard let self, self.generation == generation else { return }
                    let isReconnect = self.reconnectAttempt > 0 || self.status != .connected
                    self.reconnectAttempt = 0
                    self.status = .connected
                    do {
                        let snapshot: WorkspaceSnapshot = try await client.request(
                            ["command": .string("bootstrap_workspace")],
                            as: WorkspaceSnapshot.self
                        )
                        guard self.generation == generation else { return }
                        self.apply(snapshot, rebuildTerminals: isReconnect)
                    } catch {
                        guard self.generation == generation else { return }
                        self.errorMessage = "Reconnected, but workspace refresh failed: \(error.localizedDescription)"
                    }
                }
            },
            onEvent: { [weak self] event in
                Task { @MainActor in
                    guard let self, self.generation == generation else { return }
                    self.apply(event)
                }
            },
            onDisconnect: { [weak self] error in
                Task { @MainActor in
                    guard let self, self.generation == generation else { return }
                    self.scheduleReconnect(
                        client: client,
                        generation: generation,
                        after: error
                    )
                }
            }
        )
    }

    private func scheduleReconnect(
        client: DaemonIPCClient,
        generation: UUID,
        after error: Error
    ) {
        guard self.generation == generation, reconnectTask == nil else { return }
        status = .failed(error.localizedDescription)
        let exponent = min(reconnectAttempt, 4)
        let delayMilliseconds = min(750 * (1 << exponent), 10_000)
        reconnectAttempt += 1

        reconnectTask = Task { [weak self] in
            do {
                try await Task.sleep(for: .milliseconds(delayMilliseconds))
            } catch {
                return
            }
            guard let self, self.generation == generation, !Task.isCancelled else { return }
            do {
                try await client.connectOrLaunch()
                guard self.generation == generation else { return }
                self.reconnectTask = nil
                self.beginSubscription(client: client, generation: generation)
            } catch {
                guard self.generation == generation else { return }
                self.reconnectTask = nil
                self.scheduleReconnect(client: client, generation: generation, after: error)
            }
        }
    }

    private func apply(_ snapshot: WorkspaceSnapshot, rebuildTerminals: Bool = false) {
        let oldSelection = selectedPaneID
        panes = snapshot.panes
        paneStates = snapshot.paneStates
        agentStates = snapshot.agentStates
        agentSpecs = snapshot.agentSpecs
        layout = PaneLayout.reconcile(PaneLayout.parse(snapshot.layout), paneIDs: snapshot.panes.map(\.id))

        let liveIDs = Set(snapshot.panes.map(\.id))
        terminals = terminals.filter { liveIDs.contains($0.key) }
        chats = chats.filter { liveIDs.contains($0.key) }
        if rebuildTerminals {
            // A subscription outage can drop PTY events while the daemon and
            // shell keep running. Recreate the emulator from the authoritative
            // scrollback tail so the reattached view catches up instead of
            // silently preserving a stale screen.
            terminals = [:]
        }

        for pane in snapshot.panes {
            if pane.kind == .shell {
                let surface = terminal(for: pane.id, size: snapshot.sizes[pane.id])
                surface.loadInitialScrollback(snapshot.scrollback[pane.id] ?? "")
                if let client { ensureTerminal(pane.id, client: client) }
            } else {
                // User prompts are rendered locally because the daemon does
                // not echo them. Fold replay into an existing chat so a close,
                // reconnect, or other snapshot refresh cannot erase them;
                // per-event sequence numbers discard the overlapping tail.
                var chat = chats[pane.id] ?? AgentChatState()
                chat.replay(snapshot.agentEvents[pane.id, default: []])
                if snapshot.paneStates[pane.id] == .ended { chat.markPaneEnded() }
                chats[pane.id] = chat
            }
        }

        if let oldSelection, liveIDs.contains(oldSelection) {
            selectedPaneID = oldSelection
        } else {
            selectedPaneID = snapshot.activePaneID ?? snapshot.panes.first?.id
        }
    }

    private func apply(_ event: DaemonEvent) {
        switch event.kind {
        case "pty_output":
            guard let paneID = event["pane_id"]?.stringValue,
                  let data = event["data"]?.stringValue
            else { return }
            terminal(for: paneID).feed(data)

        case "pane_ended":
            guard let paneID = event["pane_id"]?.stringValue else { return }
            paneStates[paneID] = .ended
            if panes.first(where: { $0.id == paneID })?.kind == .agent {
                var chat = chats[paneID] ?? AgentChatState()
                chat.markPaneEnded(exitCode: event["exit_code"]?.numberValue.map(Int.init))
                chats[paneID] = chat
            }

        case "pane_created", "pane_renamed":
            guard let value = event["pane"],
                  let data = try? JSONEncoder.ipc.encode(value),
                  let pane = try? JSONDecoder.ipc.decode(Pane.self, from: data)
            else { return }
            upsert(pane)

        case "pane_closed":
            guard let paneID = event["pane_id"]?.stringValue else { return }
            remove(paneID)

        case "agent_state":
            guard let paneID = event["pane_id"]?.stringValue else { return }
            let attention = event["attention"]?.stringValue.flatMap(AgentAttention.init(rawValue:))
            agentStates[paneID] = AgentPaneInfo(agent: event["agent"]?.stringValue, attention: attention)

        case "agent_event":
            guard let paneID = event["pane_id"]?.stringValue,
                  let payload = event["payload"]
            else { return }
            var chat = chats[paneID] ?? AgentChatState()
            chat.apply(payload)
            chats[paneID] = chat

        default:
            break
        }
    }

    private func ensureTerminal(_ paneID: String, client: DaemonIPCClient) {
        guard ensuringTerminals.insert(paneID).inserted else { return }
        Task { [weak self] in
            guard let self else { return }
            defer { self.ensuringTerminals.remove(paneID) }
            do {
                let _: CommandOK = try await client.request([
                    "command": .string("ensure_pane_terminal"),
                    "pane_id": .string(paneID),
                ], as: CommandOK.self)
                guard self.client === client else { return }
                self.paneStates[paneID] = .live
            } catch {
                guard self.client === client else { return }
                self.terminals[paneID]?.feed(
                    "\r\n[sgian] failed to start terminal: \(error.localizedDescription)\r\n"
                )
                self.errorMessage = "Failed to start terminal: \(error.localizedDescription)"
            }
        }
    }

    private func terminal(for paneID: String, size: PaneSize? = nil) -> TerminalSurface {
        if let existing = terminals[paneID] { return existing }
        let surface = TerminalSurface(
            id: paneID,
            fontSize: terminalFontSize,
            columns: size?.cols ?? 120,
            rows: size?.rows ?? 40
        )
        surface.onInput = { [weak self] data in
            guard let self else { return }
            let text = String(decoding: data, as: UTF8.self)
            self.write(text, to: paneID)
        }
        surface.onResize = { [weak self] columns, rows in
            self?.resize(paneID, columns: columns, rows: rows)
        }
        terminals[paneID] = surface
        return surface
    }

    private func write(_ text: String, to paneID: String) {
        pendingInput[paneID, default: ""] += text
        guard inputDrains[paneID] == nil else { return }
        inputDrains[paneID] = Task { [weak self] in
            guard let self else { return }
            defer { inputDrains[paneID] = nil }
            while !Task.isCancelled {
                guard let client, let chunk = pendingInput[paneID], !chunk.isEmpty else { return }
                pendingInput[paneID] = ""
                do {
                    let _: CommandOK = try await client.request([
                        "command": .string("write_to_pane"),
                        "pane_id": .string(paneID),
                        "data": .string(chunk),
                    ], as: CommandOK.self)
                } catch {
                    // Preserve bytes that have not reached the daemon so a
                    // transient failure cannot silently reorder later input.
                    pendingInput[paneID] = chunk + pendingInput[paneID, default: ""]
                    return
                }
            }
        }
    }

    private func resize(_ paneID: String, columns: Int, rows: Int) {
        perform(reportErrors: false) { client in
            let _: CommandOK = try await client.request([
                "command": .string("resize_pane_terminal"),
                "pane_id": .string(paneID),
                "cols": .number(Double(columns)),
                "rows": .number(Double(rows)),
            ], as: CommandOK.self)
        }
    }

    private func upsert(_ pane: Pane) {
        if let index = panes.firstIndex(where: { $0.id == pane.id }) {
            panes[index] = pane
        } else {
            panes.append(pane)
        }
        paneStates[pane.id] = paneStates[pane.id] ?? .live
        layout = PaneLayout.reconcile(layout, paneIDs: panes.map(\.id))
        if pane.kind == .shell {
            _ = terminal(for: pane.id)
            if let client { ensureTerminal(pane.id, client: client) }
        }
        if pane.kind == .agent { chats[pane.id] = chats[pane.id] ?? AgentChatState() }
    }

    private func remove(_ paneID: String) {
        panes.removeAll { $0.id == paneID }
        paneStates.removeValue(forKey: paneID)
        agentStates.removeValue(forKey: paneID)
        agentSpecs.removeValue(forKey: paneID)
        terminals.removeValue(forKey: paneID)
        chats.removeValue(forKey: paneID)
        layout = layout?.removing(paneID)
        if selectedPaneID == paneID { selectedPaneID = panes.first?.id }
    }

    private func perform(
        reportErrors: Bool = true,
        _ operation: @escaping (DaemonIPCClient) async throws -> Void
    ) {
        guard let client else { return }
        Task { [weak self] in
            guard let self, self.client === client else { return }
            do { try await operation(client) }
            catch { if reportErrors, self.client === client { present(error) } }
        }
    }

    private func present(_ error: Error) {
        errorMessage = error.localizedDescription
    }

    private var uiSmokeRequested: Bool {
        guard let value = ProcessInfo.processInfo.environment["SGIAN_UI_SMOKE"] else { return false }
        return ["1", "true", "yes"].contains(value.trimmingCharacters(in: .whitespacesAndNewlines).lowercased())
    }

    private var uiSmokeMarkerURL: URL {
        if let path = ProcessInfo.processInfo.environment["SGIAN_UI_SMOKE_MARKER"]?
            .trimmingCharacters(in: .whitespacesAndNewlines),
           !path.isEmpty
        {
            return URL(fileURLWithPath: path)
        }
        return workspaceURL.appendingPathComponent(".sgian-ui-smoke-ok")
    }

    private func completeUISmokeIfRequested() async throws {
        guard uiSmokeRequested else { return }
        // Let SwiftUI install the selected detail view so this checks the
        // packaged application lifecycle, not only the IPC bootstrap.
        for _ in 0..<50 {
            if let pane = selectedPane,
               pane.kind == .shell,
               terminals[pane.id]?.view.window != nil
            {
                let marker = uiSmokeMarkerURL
                try FileManager.default.createDirectory(
                    at: marker.deletingLastPathComponent(),
                    withIntermediateDirectories: true
                )
                try Data("ok\n".utf8).write(to: marker, options: .atomic)
                NSApplication.shared.terminate(nil)
                return
            }
            try await Task.sleep(for: .milliseconds(100))
        }
        throw DaemonClientError.request("Native UI smoke did not render a shell pane")
    }

    private func failUISmokeIfRequested(_ error: Error) {
        guard uiSmokeRequested else { return }
        let marker = uiSmokeMarkerURL
        let errorURL = URL(fileURLWithPath: marker.path + ".err")
        do {
            try FileManager.default.createDirectory(
                at: marker.deletingLastPathComponent(),
                withIntermediateDirectories: true
            )
            try? FileManager.default.removeItem(at: marker)
            try Data("\(error.localizedDescription)\n".utf8).write(to: errorURL, options: .atomic)
        } catch {
            fputs("native UI smoke failed: \(error.localizedDescription)\n", stderr)
        }
        NSApplication.shared.terminate(nil)
    }
}
