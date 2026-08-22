import Foundation

struct ChatMessage: Identifiable, Equatable {
    enum Kind: Equatable {
        case user
        case assistant
        case tool
        case toolResult
        case error
        case historyElided
    }

    let id: UUID
    var kind: Kind
    var text: String
    var title: String?
    var detail: String?
    var toolUseID: String?
    var isError: Bool
    var isOpen: Bool

    init(
        id: UUID = UUID(),
        kind: Kind,
        text: String = "",
        title: String? = nil,
        detail: String? = nil,
        toolUseID: String? = nil,
        isError: Bool = false,
        isOpen: Bool = false
    ) {
        self.id = id
        self.kind = kind
        self.text = text
        self.title = title
        self.detail = detail
        self.toolUseID = toolUseID
        self.isError = isError
        self.isOpen = isOpen
    }
}

struct PendingPermission: Equatable {
    let requestID: String
    let toolName: String
    let input: JSONValue?
}

struct AgentTurnSummary: Equatable {
    let subtype: String?
    let costUSD: Double?
    let durationMS: Double?

    var label: String {
        var parts: [String] = []
        if let subtype, !subtype.isEmpty { parts.append(subtype.replacingOccurrences(of: "_", with: " ")) }
        if let costUSD { parts.append(String(format: "$%.4f", costUSD)) }
        if let durationMS { parts.append(String(format: "%.1fs", durationMS / 1000)) }
        return parts.joined(separator: " · ")
    }
}

struct AgentChatState: Equatable {
    private static let maximumMessages = 500

    var sessionID: String?
    var model: String?
    var messages: [ChatMessage] = []
    var busy = false
    var pendingPermission: PendingPermission?
    var lastTurn: AgentTurnSummary?
    var exited = false
    var exitCode: Int?
    var lastSequence: Double = 0

    mutating func replay(_ events: [JSONValue]) {
        events.forEach { apply($0) }
    }

    mutating func apply(_ event: JSONValue) {
        guard let object = event.objectValue,
              let kind = object["kind"]?.stringValue
        else { return }

        if let sequence = object["seq"]?.numberValue {
            guard sequence > lastSequence else { return }
            lastSequence = sequence
        }

        switch kind {
        case "session":
            sessionID = object["session_id"]?.stringValue ?? sessionID
            model = object["model"]?.stringValue ?? model
            exited = false
            exitCode = nil

        case "message_start":
            closeOpenAssistant()
            messages.append(ChatMessage(kind: .assistant, isOpen: true))
            busy = true

        case "text_delta":
            guard let text = object["text"]?.stringValue, !text.isEmpty else { break }
            if let index = messages.lastIndex(where: { $0.kind == .assistant && $0.isOpen }) {
                messages[index].text += text
            } else {
                messages.append(ChatMessage(kind: .assistant, text: text, isOpen: true))
            }
            busy = true

        case "message_complete":
            closeOpenAssistant()

        case "tool_use":
            closeOpenAssistant()
            messages.append(ChatMessage(
                kind: .tool,
                title: object["name"]?.stringValue ?? "Tool",
                detail: object["input"]?.prettyPrinted,
                toolUseID: object["id"]?.stringValue
            ))

        case "tool_result":
            let toolID = object["tool_use_id"]?.stringValue
            let content = Self.normalizedToolContent(object["content"])
            let isError = object["is_error"]?.boolValue == true
            if let index = messages.lastIndex(where: { $0.kind == .tool && $0.toolUseID == toolID && $0.text.isEmpty }) {
                messages[index].text = content
                messages[index].isError = isError
            } else {
                messages.append(ChatMessage(kind: .toolResult, text: content, toolUseID: toolID, isError: isError))
            }

        case "permission_request":
            guard let requestID = object["request_id"]?.stringValue else { break }
            pendingPermission = PendingPermission(
                requestID: requestID,
                toolName: object["tool_name"]?.stringValue ?? "Tool",
                input: object["input"]
            )

        case "permission_resolved":
            if object["request_id"]?.stringValue == pendingPermission?.requestID {
                pendingPermission = nil
                if ["closed", "process_exit"].contains(object["reason"]?.stringValue) { busy = false }
            }

        case "turn_complete":
            closeOpenAssistant()
            busy = false
            pendingPermission = nil
            lastTurn = AgentTurnSummary(
                subtype: object["subtype"]?.stringValue,
                costUSD: object["cost_usd"]?.numberValue,
                durationMS: object["duration_ms"]?.numberValue
            )

        case "error":
            messages.append(ChatMessage(
                kind: .error,
                text: object["message"]?.stringValue ?? "Agent error",
                isError: true
            ))

        case "process_exit":
            closeOpenAssistant()
            busy = false
            pendingPermission = nil
            exited = true
            exitCode = object["exit_code"]?.numberValue.map(Int.init)

        default:
            break
        }
        capMessages()
    }

    mutating func appendUserMessage(_ text: String) {
        messages.append(ChatMessage(kind: .user, text: text))
        busy = true
        capMessages()
    }

    mutating func removeLastUserMessage(matching text: String) {
        guard let index = messages.lastIndex(where: { $0.kind == .user && $0.text == text }) else { return }
        messages.remove(at: index)
        busy = false
    }

    mutating func markPaneEnded(exitCode: Int? = nil) {
        closeOpenAssistant()
        busy = false
        pendingPermission = nil
        exited = true
        self.exitCode = exitCode
    }

    private mutating func closeOpenAssistant() {
        guard let index = messages.lastIndex(where: { $0.kind == .assistant && $0.isOpen }) else { return }
        messages[index].isOpen = false
    }

    private mutating func capMessages() {
        guard messages.count > Self.maximumMessages else { return }
        let removed = messages.count - Self.maximumMessages + 1
        messages.removeFirst(removed)
        messages.insert(
            ChatMessage(kind: .historyElided, text: "\(removed) earlier messages are not shown"),
            at: 0
        )
    }

    private static func normalizedToolContent(_ value: JSONValue?) -> String {
        guard let value else { return "" }
        switch value {
        case let .string(text): return text
        case let .array(blocks):
            return blocks.compactMap { block in
                block.stringValue ?? block["text"]?.stringValue ?? block.prettyPrinted
            }.joined(separator: "\n")
        case .null: return ""
        default: return value.prettyPrinted
        }
    }
}
