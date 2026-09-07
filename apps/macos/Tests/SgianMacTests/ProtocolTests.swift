import Foundation
import Testing

@testable import SgianMac

@Test func userPromptsSurviveReplayWithoutDuplicatingOptimisticMessages() throws {
    let event = try JSONDecoder().decode(JSONValue.self, from: Data(#"{"kind":"user_message","text":"hello","message_id":"send-1","seq":1}"#.utf8))
    var chat = AgentChatState()
    chat.appendUserMessage("hello", messageID: "send-1")
    chat.apply(event)
    chat.replay([event])
    chat.removeLastUserMessage(matching: "hello", messageID: "send-1")
    #expect(chat.messages.count == 1)
    #expect(chat.messages[0].isPending == false)
    var reopened = AgentChatState()
    reopened.replay([event])
    #expect(reopened.messages.first?.text == "hello")
    let other = try JSONDecoder().decode(JSONValue.self, from: Data(#"{"kind":"user_message","text":"hello","message_id":"send-2","seq":2}"#.utf8))
    reopened.apply(other)
    #expect(reopened.messages.count == 2)
    reopened.appendUserMessage("failed", messageID: "send-3")
    reopened.removeLastUserMessage(matching: "failed", messageID: "send-3")
    #expect(reopened.messages.count == 2)
}

@Test func terminalLinksOnlyOpenWebURLs() {
    #expect(TerminalLinkPolicy.externalURL("https://example.com/docs") != nil)
    #expect(TerminalLinkPolicy.externalURL("http://localhost:8080/") != nil)
    for link in ["file:///Applications/Calculator.app", "javascript:alert(1)", "data:text/html,test",
                 "ssh://example.com", "custom-app://run", "https://trusted.example@evil.example/", "/relative"] {
        #expect(TerminalLinkPolicy.externalURL(link) == nil)
    }
}

@Test func fnvWorkspaceKeysMatchTheRustAlgorithm() {
    #expect(WorkspaceLocator.fnvWorkspaceKey("") == "cbf29ce484222325")
    #expect(WorkspaceLocator.fnvWorkspaceKey("a") == "af63dc4c8601ec8c")
    #expect(WorkspaceLocator.fnvWorkspaceKey("hello") == "a430d84680aabd0b")
}

@Test func locatorUsesPOSIXCanonicalPath() throws {
    let root = FileManager.default.temporaryDirectory
        .appendingPathComponent("sgian-native-locator-\(UUID().uuidString)", isDirectory: true)
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: root) }

    var buffer = [CChar](repeating: 0, count: Int(PATH_MAX))
    #expect(realpath(root.path, &buffer) != nil)
    let canonical = String(cString: buffer)
    let locator = WorkspaceLocator(workspaceURL: root)
    #expect(locator.workspaceURL.path == canonical)
    #expect(locator.workspaceKey == WorkspaceLocator.fnvWorkspaceKey(canonical))
}

@Test func locatorFallsBackToLegacyDirectoryByPersistedWorkspacePath() throws {
    let root = FileManager.default.temporaryDirectory
        .appendingPathComponent("sgian-native-legacy-\(UUID().uuidString)", isDirectory: true)
    let workspace = root.appendingPathComponent("project", isDirectory: true)
    let workspacesRoot = root.appendingPathComponent("workspaces", isDirectory: true)
    let legacy = workspacesRoot.appendingPathComponent("legacy-hash", isDirectory: true)
    try FileManager.default.createDirectory(at: workspace, withIntermediateDirectories: true)
    try FileManager.default.createDirectory(at: legacy, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: root) }

    let state = try JSONSerialization.data(withJSONObject: ["cwd": workspace.path])
    try state.write(to: legacy.appendingPathComponent("workspace.json"))
    let key = WorkspaceLocator.fnvWorkspaceKey(workspace.path)
    let resolved = WorkspaceLocator.resolveDataDirectory(
        workspacesRoot: workspacesRoot,
        workspaceURL: workspace,
        workspaceKey: key
    )
    #expect(resolved.resolvingSymlinksInPath() == legacy.resolvingSymlinksInPath())

    let current = workspacesRoot.appendingPathComponent(key, isDirectory: true)
    try FileManager.default.createDirectory(at: current, withIntermediateDirectories: true)
    #expect(
        WorkspaceLocator.resolveDataDirectory(
            workspacesRoot: workspacesRoot,
            workspaceURL: workspace,
            workspaceKey: key
        ) == current
    )
}

@Test func bootstrapDecodesLegacyOptionalFields() throws {
    let payload = Data(#"{"panes":[{"id":"pane-1","title":"term-1","kind":"shell","created_at_ms":1}],"active_pane_id":"pane-1","cwd":"/tmp"}"#.utf8)
    let snapshot = try JSONDecoder.ipc.decode(WorkspaceSnapshot.self, from: payload)
    #expect(snapshot.panes.count == 1)
    #expect(snapshot.scrollback.isEmpty)
    #expect(snapshot.sizes.isEmpty)
    #expect(snapshot.agentEvents.isEmpty)
    #expect(snapshot.agentSpecs.isEmpty)
}

@Test func agentReducerStreamsAndDeduplicatesEvents() {
    var chat = AgentChatState()
    chat.apply(.object(["kind": .string("message_start"), "seq": .number(1)]))
    chat.apply(.object(["kind": .string("text_delta"), "text": .string("hello"), "seq": .number(2)]))
    chat.apply(.object(["kind": .string("text_delta"), "text": .string(" duplicated"), "seq": .number(2)]))
    chat.apply(.object(["kind": .string("message_complete"), "seq": .number(3)]))

    #expect(chat.messages.count == 1)
    #expect(chat.messages[0].text == "hello")
    #expect(chat.messages[0].isOpen == false)
    #expect(chat.busy == true)

    chat.apply(.object([
        "kind": .string("turn_complete"),
        "seq": .number(4),
        "subtype": .string("success"),
        "cost_usd": .number(0.0123),
    ]))
    #expect(chat.busy == false)
    #expect(chat.lastTurn?.label.contains("$0.0123") == true)
}

@Test func agentReducerTracksPermissionsByRequestID() {
    var chat = AgentChatState()
    chat.apply(.object([
        "kind": .string("permission_request"),
        "request_id": .string("request-1"),
        "tool_name": .string("Bash"),
        "input": .object(["command": .string("git status")]),
    ]))
    chat.apply(.object([
        "kind": .string("permission_resolved"),
        "request_id": .string("older-request"),
    ]))
    #expect(chat.pendingPermission?.requestID == "request-1")

    chat.apply(.object([
        "kind": .string("permission_resolved"),
        "request_id": .string("request-1"),
        "reason": .string("user"),
    ]))
    #expect(chat.pendingPermission == nil)
}

@Test func agentReplayPreservesLocalPromptsAndAppliesOnlyNewEvents() {
    var chat = AgentChatState()
    chat.appendUserMessage("keep this prompt")
    chat.replay([
        .object(["kind": .string("message_start"), "seq": .number(1)]),
        .object([
            "kind": .string("text_delta"),
            "text": .string("first"),
            "seq": .number(2),
        ]),
    ])

    chat.replay([
        .object(["kind": .string("message_start"), "seq": .number(1)]),
        .object([
            "kind": .string("text_delta"),
            "text": .string("first"),
            "seq": .number(2),
        ]),
        .object([
            "kind": .string("text_delta"),
            "text": .string(" second"),
            "seq": .number(3),
        ]),
        .object(["kind": .string("message_complete"), "seq": .number(4)]),
        .object(["kind": .string("turn_complete"), "seq": .number(5)]),
    ])

    #expect(chat.messages.count == 2)
    #expect(chat.messages[0].kind == .user)
    #expect(chat.messages[0].text == "keep this prompt")
    #expect(chat.messages[1].kind == .assistant)
    #expect(chat.messages[1].text == "first second")
    #expect(chat.busy == false)
    #expect(chat.lastSequence == 5)
}

@Test(
    "Native client performs a real daemon round-trip",
    .enabled(if: ProcessInfo.processInfo.environment["SGIAN_NATIVE_INTEGRATION"] == "1")
)
func daemonRoundTrip() async throws {
    let root = FileManager.default.temporaryDirectory
        .appendingPathComponent("sgian-native-integration-\(UUID().uuidString)", isDirectory: true)
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: root) }

    let locator = WorkspaceLocator(workspaceURL: root)
    defer {
        try? FileManager.default.removeItem(at: locator.dataDirectoryURL)
        try? FileManager.default.removeItem(at: locator.runtimeDirectoryURL)
    }
    let client = DaemonIPCClient(locator: locator)
    try await client.connectOrLaunch()
    let snapshot: WorkspaceSnapshot = try await client.request(
        ["command": .string("bootstrap_workspace")],
        as: WorkspaceSnapshot.self
    )
    #expect(snapshot.panes.count == 1)

    let pane: Pane = try await client.request([
        "command": .string("create_pane"),
        "title": .string("native-test"),
    ], as: Pane.self)
    #expect(pane.title == "native-test")

    let resized: CommandOK = try await client.request([
        "command": .string("resize_pane_terminal"),
        "pane_id": .string(pane.id),
        "cols": .number(101),
        "rows": .number(31),
    ], as: CommandOK.self)
    #expect(resized.ok)
    let rawSnapshot: JSONValue = try await client.request(
        ["command": .string("bootstrap_workspace")],
        as: JSONValue.self
    )
    #expect(rawSnapshot["sizes"]?[pane.id]?["cols"]?.numberValue == 101)
    let resizedData = try JSONEncoder.ipc.encode(rawSnapshot)
    let resizedSnapshot = try JSONDecoder.ipc.decode(WorkspaceSnapshot.self, from: resizedData)
    #expect(resizedSnapshot.sizes[pane.id] == PaneSize(cols: 101, rows: 31))

    let stopped: CommandOK = try await client.request(
        ["command": .string("shutdown")],
        as: CommandOK.self
    )
    #expect(stopped.ok)
}
