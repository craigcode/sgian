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


@Test func workspaceSnapshotDecodesHeldLeasesAndToleratesTheirAbsence() throws {
    let with = try JSONDecoder().decode(WorkspaceSnapshot.self, from: Data(#"{"panes":[],"cwd":"/w","leases":{"pane-1":{"holder":"bob","since_ms":42}}}"#.utf8))
    #expect(with.leases["pane-1"] == LeaseInfo(holder: "bob", sinceMs: 42))
    let without = try JSONDecoder().decode(WorkspaceSnapshot.self, from: Data(#"{"panes":[],"cwd":"/w"}"#.utf8))
    #expect(without.leases.isEmpty)
}

@Test func leaseStateEventsFoldIntoTheHeldPaneMap() throws {
    var leases: [String: LeaseInfo] = [:]
    let taken = try JSONDecoder().decode(JSONValue.self, from: Data(#"{"event":"lease_state","pane_id":"pane-1","transition":"taken","holder":"amy","since_ms":7}"#.utf8))
    #expect(LeaseInfo.apply(event: taken, to: &leases))
    #expect(leases["pane-1"] == LeaseInfo(holder: "amy", sinceMs: 7))
    let released = try JSONDecoder().decode(JSONValue.self, from: Data(#"{"event":"lease_state","pane_id":"pane-1","transition":"released","holder":null,"note":"done"}"#.utf8))
    #expect(LeaseInfo.apply(event: released, to: &leases))
    #expect(leases.isEmpty)
    let malformed = try JSONDecoder().decode(JSONValue.self, from: Data(#"{"event":"lease_state"}"#.utf8))
    #expect(LeaseInfo.apply(event: malformed, to: &leases) == false)
}

@Test func leaseTextHelpersMatchTheDaemonRules() {
    #expect(LeaseText.isValidHolder("craig@mbp"))
    #expect(!LeaseText.isValidHolder("two words"))
    #expect(!LeaseText.isValidHolder(""))
    #expect(!LeaseText.isValidHolder(String(repeating: "x", count: 65)))
    #expect(LeaseText.noticeText(for: "pane keyboard is held by bob (pane-2)") == "Read-only: keyboard held by bob. ⇧⌘T to take it.")
    #expect(LeaseText.noticeText(for: "pane keyboard is unheld and lease_policy is required; take it first (pane-2)").contains("take the keyboard"))
    let holder = WorkspaceModel.defaultHolder()
    #expect(LeaseText.isValidHolder(holder))
}


@Test func agentPaneInfoDecodesModeAndUnattendedWithDefaults() throws {
    let flagged = try JSONDecoder().decode(AgentPaneInfo.self, from: Data(#"{"agent":"claude","attention":"idle","mode":"auto","unattended":true}"#.utf8))
    #expect(flagged.mode == "auto")
    #expect(flagged.isUnattended)
    let legacy = try JSONDecoder().decode(AgentPaneInfo.self, from: Data(#"{"agent":"claude","attention":"working"}"#.utf8))
    #expect(legacy.mode == nil)
    #expect(!legacy.isUnattended)
}

@Test func workspaceSnapshotDecodesProjectsAndOutputWarnings() throws {
    let json = #"{"panes":[],"cwd":"/w","projects":{"feat":{"name":"feat","goal":"ship","panes":["p1","p1","p2"],"created_at_ms":1},"bad":"no"},"output_warnings":{"p1":{"conceal":2,"c1_controls":1},"p2":{}}}"#
    let snapshot = try JSONDecoder().decode(WorkspaceSnapshot.self, from: Data(json.utf8))
    #expect(snapshot.projects["feat"] == Project(name: "feat", goal: "ship", panes: ["p1", "p2"]))
    #expect(snapshot.projects.count == 1)
    #expect(snapshot.outputWarnings["p1"] == OutputTricks(conceal: 2, c1Controls: 1))
    #expect(snapshot.outputWarnings["p1"]?.summary == "2 concealed text, 1 C1 controls")
    #expect(snapshot.outputWarnings["p2"] == nil, "a zero total is not a warning")
    let bare = try JSONDecoder().decode(WorkspaceSnapshot.self, from: Data(#"{"panes":[],"cwd":"/w"}"#.utf8))
    #expect(bare.projects.isEmpty && bare.outputWarnings.isEmpty)
}

@Test func projectsChangedAndOutputWarningEventsFoldIntoTheirMaps() throws {
    var projects: [String: Project] = ["old": Project(name: "old")]
    let changed = try JSONDecoder().decode(JSONValue.self, from: Data(#"{"event":"projects_changed","projects":{"feat":{"name":"feat","panes":["p2"]}}}"#.utf8))
    #expect(Project.apply(event: changed, to: &projects))
    #expect(projects.keys.sorted() == ["feat"], "the whole table is replaced")
    #expect(projects["feat"]?.panes == ["p2"])
    let malformed = try JSONDecoder().decode(JSONValue.self, from: Data(#"{"event":"projects_changed"}"#.utf8))
    #expect(Project.apply(event: malformed, to: &projects) == false)
    #expect(projects["feat"] != nil)

    var warnings: [String: OutputTricks] = [:]
    let warning = try JSONDecoder().decode(JSONValue.self, from: Data(#"{"event":"output_warning","pane_id":"p1","added":{"clipboard":1},"total":{"conceal":2,"clipboard":1}}"#.utf8))
    #expect(OutputTricks.apply(event: warning, to: &warnings))
    #expect(warnings["p1"]?.total == 3)
    let bad = try JSONDecoder().decode(JSONValue.self, from: Data(#"{"event":"output_warning","pane_id":"p1"}"#.utf8))
    #expect(OutputTricks.apply(event: bad, to: &warnings) == false)
    #expect(warnings["p1"]?.total == 3)
}

@Test func projectBoardGroupsPanesAndRollsAttentionUp() {
    let panes = [
        Pane(id: "p1", title: "one", kind: .shell, createdAtMs: 1),
        Pane(id: "p2", title: "two", kind: .agent, createdAtMs: 2),
        Pane(id: "p3", title: "three", kind: .shell, createdAtMs: 3),
    ]
    let projects = [
        "zeta": Project(name: "zeta", panes: ["p3", "gone"]),
        "alpha": Project(name: "alpha", goal: "first", panes: ["p2"]),
        "empty": Project(name: "empty"),
    ]
    let groups = ProjectBoard.group(panes: panes, projects: projects)
    #expect(groups.map(\.name) == ["alpha", "empty", "zeta", nil])
    #expect(groups[0].panes.map(\.id) == ["p2"])
    #expect(groups[0].goal == "first")
    #expect(groups[2].panes.map(\.id) == ["p3"])
    #expect(groups[3].panes.map(\.id) == ["p1"])
    #expect(groups[3].title == "No project")
    let all = ProjectBoard.group(panes: panes, projects: ["a": Project(name: "a", panes: ["p1", "p2", "p3"])])
    #expect(all.map(\.name) == ["a"], "no trailing group when every pane is assigned")
    let none = ProjectBoard.group(panes: panes, projects: [:])
    #expect(none.count == 1 && none[0].name == nil && none[0].panes.count == 3)

    let rollup = ProjectBoard.rollup(
        panes: panes,
        paneStates: ["p3": .ended],
        attention: { ["p1": .needsInput, "p2": .working][$0] },
        agentStates: ["p2": AgentPaneInfo(agent: "claude", attention: .working, mode: "auto", unattended: true)],
        leases: ["p1": LeaseInfo(holder: "bob"), "p2": LeaseInfo(holder: "alice"), "p3": LeaseInfo(holder: "bob")],
        outputWarnings: ["p2": OutputTricks(conceal: 1)]
    )
    #expect(rollup == ProjectRollup(panes: 3, live: 2, needsInput: 1, working: 1, idle: 0, unattended: 1, held: 3, holders: ["alice", "bob"], warnings: 1))
    #expect(rollup.text == "3 panes · 2 live · 1 needs input · 1 working · ⚠ 1 unattended · 1 with output warnings · ⌨ alice, bob")
    #expect(ProjectRollup(panes: 1, live: 1).text == "1 pane")
}

@Test func agentUsageDecodesSummarisesAndFoldsEvents() throws {
    let json = #"{"panes":[],"cwd":"/w","agent_usage":{"p1":{"model":"Opus","context_used_percentage":40,"five_hour":{"used_percentage":23,"resets_at":1000000},"seven_day":{"used_percentage":41},"updated_at_ms":5},"p2":{"updated_at_ms":1}}}"#
    let snapshot = try JSONDecoder().decode(WorkspaceSnapshot.self, from: Data(json.utf8))
    let usage = try #require(snapshot.agentUsage["p1"])
    #expect(usage.model == "Opus")
    #expect(usage.fiveHour == RateLimitWindow(usedPercentage: 23, resetsAt: 1000000))
    #expect(usage.summary(now: 1000000 - 4200) == "Opus · 40% context · 5h 23% ↻ 1h10m · 7d 41%")
    #expect(usage.summary(now: 3000000) == "Opus · 40% context · 5h 23% · 7d 41%")
    #expect(usage.isHot == false)
    #expect(snapshot.agentUsage["p2"] == nil, "an empty reading is not a usage")
    #expect(AgentUsage.formatReset(100 + 90 * 60, now: 100) == " ↻ 1h30m")
    #expect(AgentUsage.formatReset(nil, now: 0) == "")

    var table: [String: AgentUsage] = [:]
    let event = try JSONDecoder().decode(JSONValue.self, from: Data(#"{"event":"agent_usage","pane_id":"p1","usage":{"model":"Opus","context_used_percentage":85,"updated_at_ms":9}}"#.utf8))
    #expect(AgentUsage.apply(event: event, to: &table))
    #expect(table["p1"]?.contextUsedPercentage == 85)
    #expect(table["p1"]?.isHot == true)
    let malformed = try JSONDecoder().decode(JSONValue.self, from: Data(#"{"event":"agent_usage","pane_id":"p1"}"#.utf8))
    #expect(AgentUsage.apply(event: malformed, to: &table) == false)
    #expect(table["p1"] != nil)
}
