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

    enum CodingKeys: String, CodingKey {
        case panes, cwd, layout, scrollback, sizes
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
