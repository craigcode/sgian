import Foundation

enum PaneKind: String, Codable, Sendable {
    case shell
    case agent
}

enum PaneRuntimeState: String, Codable, Sendable {
    case live
    case ended
}

enum AgentAttention: String, Codable, Sendable {
    case working
    case needsInput = "needs_input"
    case idle
}

enum AgentBackend: String, Codable, CaseIterable, Identifiable, Sendable {
    case claude
    case droid

    var id: String { rawValue }
    var displayName: String { rawValue.capitalized }
}

struct Pane: Codable, Identifiable, Equatable, Sendable {
    let id: String
    var title: String
    let kind: PaneKind
    let createdAtMs: UInt64

    enum CodingKeys: String, CodingKey {
        case id, title, kind
        case createdAtMs = "created_at_ms"
    }
}

struct AgentPaneSpec: Codable, Equatable, Sendable {
    var backend: AgentBackend = .claude
    var model: String?
}

struct AgentPaneInfo: Codable, Equatable, Sendable {
    var agent: String?
    var attention: AgentAttention?
    /// The agent's observed permission mode (`auto`, `bypass`, …); nil when unknown.
    var mode: String?
    /// True when tools run without a person approving them (auto / bypass).
    var unattended: Bool?

    init(agent: String?, attention: AgentAttention?, mode: String? = nil, unattended: Bool? = nil) {
        self.agent = agent
        self.attention = attention
        self.mode = mode
        self.unattended = unattended
    }

    var isUnattended: Bool { unattended == true }
}

/// A pane's keyboard lease (docs/design/keyboard-lease-and-ledger.md).
/// `holder == nil` means unheld.
struct LeaseInfo: Codable, Equatable, Sendable {
    var holder: String?
    var sinceMs: UInt64?

    enum CodingKeys: String, CodingKey {
        case holder
        case sinceMs = "since_ms"
    }

    init(holder: String?, sinceMs: UInt64? = nil) {
        self.holder = holder
        self.sinceMs = sinceMs
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        holder = try container.decodeIfPresent(String.self, forKey: .holder)
        sinceMs = try container.decodeIfPresent(UInt64.self, forKey: .sinceMs)
    }

    /// Fold one `lease_state` event into the held-pane map: `taken` records the
    /// holder, `released`/`revoked` clear it. Returns false for a malformed event.
    @discardableResult
    static func apply(event: JSONValue, to leases: inout [String: LeaseInfo]) -> Bool {
        guard let paneID = event["pane_id"]?.stringValue,
              let transition = event["transition"]?.stringValue
        else { return false }
        if transition == "taken", let holder = event["holder"]?.stringValue, !holder.isEmpty {
            leases[paneID] = LeaseInfo(holder: holder, sinceMs: event["since_ms"]?.numberValue.map { UInt64($0) })
        } else {
            leases.removeValue(forKey: paneID)
        }
        return true
    }
}

/// A named group of panes serving one goal (ENHANCEMENTS "projects").
/// `panes` lists member ids in project order; a pane is in at most one.
struct Project: Codable, Equatable, Sendable {
    var name: String
    var goal: String?
    var repo: String?
    var panes: [String]

    init(name: String, goal: String? = nil, repo: String? = nil, panes: [String] = []) {
        self.name = name
        self.goal = goal
        self.repo = repo
        self.panes = panes
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        name = try container.decode(String.self, forKey: .name)
        goal = try container.decodeIfPresent(String.self, forKey: .goal)
        repo = try container.decodeIfPresent(String.self, forKey: .repo)
        panes = try container.decodeIfPresent([String].self, forKey: .panes) ?? []
    }

    enum CodingKeys: String, CodingKey {
        case name, goal, repo, panes
    }

    /// Decode a `projects` table (bootstrap or `projects_changed`), dropping
    /// malformed entries and de-duplicating pane ids.
    static func table(from value: JSONValue?) -> [String: Project] {
        guard let entries = value?.objectValue else { return [:] }
        var table: [String: Project] = [:]
        for (name, entry) in entries {
            guard !name.isEmpty,
                  let data = try? JSONEncoder.ipc.encode(entry),
                  var project = try? JSONDecoder.ipc.decode(Project.self, from: data)
            else { continue }
            var seen = Set<String>()
            project.panes = project.panes.filter { !$0.isEmpty && seen.insert($0).inserted }
            project.name = name
            table[name] = project
        }
        return table
    }

    /// Fold one `projects_changed` event into the table: the daemon sends the
    /// whole table, so this replaces rather than diffs. False when malformed.
    @discardableResult
    static func apply(event: JSONValue, to projects: inout [String: Project]) -> Bool {
        guard event["projects"]?.objectValue != nil else { return false }
        projects = table(from: event["projects"])
        return true
    }
}

/// Output-guard counters (docs/design/keyboard-lease-and-ledger.md §7): the
/// tricks a pane's output used to hide something from a person. Counts only
/// grow for a pane's life.
struct OutputTricks: Codable, Equatable, Sendable {
    var conceal = 0
    var clipboard = 0
    var hyperlinkMismatch = 0
    var stringControls = 0
    var c1Controls = 0

    enum CodingKeys: String, CodingKey {
        case conceal, clipboard
        case hyperlinkMismatch = "hyperlink_mismatch"
        case stringControls = "string_controls"
        case c1Controls = "c1_controls"
    }

    init(conceal: Int = 0, clipboard: Int = 0, hyperlinkMismatch: Int = 0, stringControls: Int = 0, c1Controls: Int = 0) {
        self.conceal = conceal
        self.clipboard = clipboard
        self.hyperlinkMismatch = hyperlinkMismatch
        self.stringControls = stringControls
        self.c1Controls = c1Controls
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        conceal = max(0, try container.decodeIfPresent(Int.self, forKey: .conceal) ?? 0)
        clipboard = max(0, try container.decodeIfPresent(Int.self, forKey: .clipboard) ?? 0)
        hyperlinkMismatch = max(0, try container.decodeIfPresent(Int.self, forKey: .hyperlinkMismatch) ?? 0)
        stringControls = max(0, try container.decodeIfPresent(Int.self, forKey: .stringControls) ?? 0)
        c1Controls = max(0, try container.decodeIfPresent(Int.self, forKey: .c1Controls) ?? 0)
    }

    var total: Int { conceal + clipboard + hyperlinkMismatch + stringControls + c1Controls }

    /// "2 concealed text, 1 clipboard writes": the non-zero counters in display order.
    var summary: String {
        [
            (conceal, "concealed text"),
            (clipboard, "clipboard writes"),
            (hyperlinkMismatch, "mismatched links"),
            (stringControls, "opaque control strings"),
            (c1Controls, "C1 controls"),
        ]
        .filter { $0.0 > 0 }
        .map { "\($0.0) \($0.1)" }
        .joined(separator: ", ")
    }

    /// Fold one `output_warning` event into the per-pane map (its `total` is
    /// the running count). False when malformed.
    @discardableResult
    static func apply(event: JSONValue, to warnings: inout [String: OutputTricks]) -> Bool {
        guard let paneID = event["pane_id"]?.stringValue, !paneID.isEmpty,
              let total = event["total"],
              let data = try? JSONEncoder.ipc.encode(total),
              let tricks = try? JSONDecoder.ipc.decode(OutputTricks.self, from: data)
        else { return false }
        if tricks.total > 0 {
            warnings[paneID] = tricks
        } else {
            warnings.removeValue(forKey: paneID)
        }
        return true
    }
}

/// One rate-limit window from Claude Code's status line: percent used and
/// when it resets (unix seconds).
struct RateLimitWindow: Codable, Equatable, Sendable {
    var usedPercentage: Int
    var resetsAt: UInt64?

    enum CodingKeys: String, CodingKey {
        case usedPercentage = "used_percentage"
        case resetsAt = "resets_at"
    }

    init(usedPercentage: Int, resetsAt: UInt64? = nil) {
        self.usedPercentage = usedPercentage
        self.resetsAt = resetsAt
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        usedPercentage = min(100, max(0, try container.decodeIfPresent(Int.self, forKey: .usedPercentage) ?? 0))
        resetsAt = try container.decodeIfPresent(UInt64.self, forKey: .resetsAt)
    }
}

/// What the Claude Code session under a pane last said about itself through
/// `sgian ctl statusline`: model, context fill and the account's rate-limit
/// windows. Push from the CLI; `updatedAtMs` says how fresh it is.
struct AgentUsage: Codable, Equatable, Sendable {
    var model: String?
    var contextUsedPercentage: Int?
    var fiveHour: RateLimitWindow?
    var sevenDay: RateLimitWindow?
    var updatedAtMs: UInt64

    enum CodingKeys: String, CodingKey {
        case model
        case contextUsedPercentage = "context_used_percentage"
        case fiveHour = "five_hour"
        case sevenDay = "seven_day"
        case updatedAtMs = "updated_at_ms"
    }

    init(model: String? = nil, contextUsedPercentage: Int? = nil, fiveHour: RateLimitWindow? = nil, sevenDay: RateLimitWindow? = nil, updatedAtMs: UInt64 = 0) {
        self.model = model
        self.contextUsedPercentage = contextUsedPercentage
        self.fiveHour = fiveHour
        self.sevenDay = sevenDay
        self.updatedAtMs = updatedAtMs
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        model = try container.decodeIfPresent(String.self, forKey: .model).flatMap { $0.isEmpty ? nil : $0 }
        contextUsedPercentage = try container.decodeIfPresent(Int.self, forKey: .contextUsedPercentage).map { min(100, max(0, $0)) }
        fiveHour = try container.decodeIfPresent(RateLimitWindow.self, forKey: .fiveHour)
        sevenDay = try container.decodeIfPresent(RateLimitWindow.self, forKey: .sevenDay)
        updatedAtMs = try container.decodeIfPresent(UInt64.self, forKey: .updatedAtMs) ?? 0
    }

    var isEmpty: Bool { model == nil && contextUsedPercentage == nil && fiveHour == nil && sevenDay == nil }

    /// " ↻ 1h10m" until a window resets, or "" when unknown or past.
    static func formatReset(_ resetsAt: UInt64?, now: UInt64) -> String {
        guard let resetsAt, resetsAt > now else { return "" }
        let secs = resetsAt - now
        let h = secs / 3600
        let m = (secs % 3600) / 60
        if h >= 48 { return " ↻ \(h / 24)d" }
        if h > 0 { return " ↻ \(h)h\(String(format: "%02d", m))m" }
        return " ↻ \(m)m"
    }

    /// "Opus · 40% context · 5h 23% ↻ 1h10m · 7d 41%": the same line the
    /// daemon's `ctl agent` prints, so every surface agrees.
    func summary(now: UInt64 = UInt64(Date().timeIntervalSince1970)) -> String {
        var parts: [String] = []
        if let model { parts.append(model) }
        if let contextUsedPercentage { parts.append("\(contextUsedPercentage)% context") }
        if let fiveHour { parts.append("5h \(fiveHour.usedPercentage)%" + Self.formatReset(fiveHour.resetsAt, now: now)) }
        if let sevenDay { parts.append("7d \(sevenDay.usedPercentage)%" + Self.formatReset(sevenDay.resetsAt, now: now)) }
        return parts.joined(separator: " · ")
    }

    /// Context fill or the tightest rate-limit window is at or past 80%.
    var isHot: Bool {
        (contextUsedPercentage ?? 0) >= 80 || (fiveHour?.usedPercentage ?? 0) >= 80 || (sevenDay?.usedPercentage ?? 0) >= 80
    }

    /// Fold one `agent_usage` event into the per-pane map. False when malformed.
    @discardableResult
    static func apply(event: JSONValue, to usage: inout [String: AgentUsage]) -> Bool {
        guard let paneID = event["pane_id"]?.stringValue, !paneID.isEmpty,
              let value = event["usage"],
              let data = try? JSONEncoder.ipc.encode(value),
              let decoded = try? JSONDecoder.ipc.decode(AgentUsage.self, from: data)
        else { return false }
        if decoded.isEmpty {
            usage.removeValue(forKey: paneID)
        } else {
            usage[paneID] = decoded
        }
        return true
    }
}

struct PaneSize: Codable, Equatable, Sendable {
    var cols: Int
    var rows: Int
}

struct WorkspaceSnapshot: Codable, Sendable {
    var panes: [Pane]
    var activePaneID: String?
    var cwd: String
    var layout: JSONValue?
    var scrollback: [String: String]
    var sizes: [String: PaneSize]
    var paneStates: [String: PaneRuntimeState]
    var agentStates: [String: AgentPaneInfo]
    var agentEvents: [String: [JSONValue]]
    var agentSpecs: [String: AgentPaneSpec]
    /// Held keyboard leases only; absent from pre-lease daemons.
    var leases: [String: LeaseInfo]
    /// Projects by name; absent from pre-project daemons.
    var projects: [String: Project]
    /// Panes whose output hid something; absent from pre-guard daemons.
    var outputWarnings: [String: OutputTricks]
    /// Per-pane usage from Claude Code's status line; absent until reported.
    var agentUsage: [String: AgentUsage]

    enum CodingKeys: String, CodingKey {
        case panes, cwd, layout, scrollback, sizes, leases, projects
        case outputWarnings = "output_warnings"
        case agentUsage = "agent_usage"
        case activePaneID = "active_pane_id"
        case paneStates = "pane_states"
        case agentStates = "agent_states"
        case agentEvents = "agent_events"
        case agentSpecs = "agent_specs"
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        panes = try container.decode([Pane].self, forKey: .panes)
        activePaneID = try container.decodeIfPresent(String.self, forKey: .activePaneID)
        cwd = try container.decode(String.self, forKey: .cwd)
        layout = try container.decodeIfPresent(JSONValue.self, forKey: .layout)
        scrollback = try container.decodeIfPresent([String: String].self, forKey: .scrollback) ?? [:]
        sizes = try container.decodeIfPresent([String: PaneSize].self, forKey: .sizes) ?? [:]
        paneStates = try container.decodeIfPresent([String: PaneRuntimeState].self, forKey: .paneStates) ?? [:]
        agentStates = try container.decodeIfPresent([String: AgentPaneInfo].self, forKey: .agentStates) ?? [:]
        agentEvents = try container.decodeIfPresent([String: [JSONValue]].self, forKey: .agentEvents) ?? [:]
        agentSpecs = try container.decodeIfPresent([String: AgentPaneSpec].self, forKey: .agentSpecs) ?? [:]
        leases = try container.decodeIfPresent([String: LeaseInfo].self, forKey: .leases) ?? [:]
        projects = Project.table(from: try container.decodeIfPresent(JSONValue.self, forKey: .projects))
        let warnings = try container.decodeIfPresent([String: OutputTricks].self, forKey: .outputWarnings) ?? [:]
        outputWarnings = warnings.filter { $0.value.total > 0 }
        let usage = try container.decodeIfPresent([String: AgentUsage].self, forKey: .agentUsage) ?? [:]
        agentUsage = usage.filter { !$0.value.isEmpty }
    }
}

struct CommandOK: Codable, Sendable {
    let ok: Bool
}

struct IPCResponse: Codable, Sendable {
    let ok: Bool
    let result: JSONValue
    let error: String?
}

struct DaemonEvent: Decodable, Sendable {
    let kind: String
    let payload: [String: JSONValue]

    init(from decoder: Decoder) throws {
        let value = try JSONValue(from: decoder)
        guard case var .object(object) = value,
              let kind = object.removeValue(forKey: "event")?.stringValue
        else {
            throw DecodingError.dataCorrupted(
                .init(codingPath: decoder.codingPath, debugDescription: "daemon event is missing its event tag")
            )
        }
        self.kind = kind
        self.payload = object
    }

    subscript(key: String) -> JSONValue? { payload[key] }
}

enum ConnectionStatus: Equatable {
    case disconnected
    case connecting
    case connected
    case failed(String)

    var label: String {
        switch self {
        case .disconnected: "Disconnected"
        case .connecting: "Connecting"
        case .connected: "Connected"
        case .failed: "Connection failed"
        }
    }
}
