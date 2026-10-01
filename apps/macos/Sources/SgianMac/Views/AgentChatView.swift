import SwiftUI

struct AgentChatView: View {
    @ObservedObject var model: WorkspaceModel
    let pane: Pane
    @State private var draft = ""

    private var chat: AgentChatState { model.chats[pane.id] ?? AgentChatState() }

    var body: some View {
        VStack(spacing: 0) {
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 16) {
                        if chat.messages.isEmpty && !chat.exited {
                            EmptyChatView()
                        }
                        ForEach(chat.messages) { message in
                            ChatMessageView(message: message).id(message.id)
                        }
                        if let permission = chat.pendingPermission {
                            PermissionCard(model: model, paneID: pane.id, permission: permission)
                                .id("permission")
                        }
                        if let footer = chat.lastTurn?.label, !footer.isEmpty, !chat.busy {
                            Text(footer)
                                .font(.caption.monospacedDigit())
                                .foregroundStyle(.secondary)
                                .frame(maxWidth: .infinity, alignment: .center)
                        }
                        if chat.exited {
                            Label("Agent exited — send a message to restart it", systemImage: "arrow.clockwise")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                                .frame(maxWidth: .infinity, alignment: .center)
                        }
                        Color.clear.frame(height: 1).id("bottom")
                    }
                    .padding(.horizontal, 28)
                    .padding(.vertical, 24)
                    .frame(maxWidth: 850)
                    .frame(maxWidth: .infinity)
                }
                .onChange(of: scrollToken) { _ in
                    withAnimation(.easeOut(duration: 0.15)) { proxy.scrollTo("bottom", anchor: .bottom) }
                }
            }

            Divider()
            composer
        }
        .background(Color(nsColor: .textBackgroundColor).opacity(0.35))
    }

    private var composer: some View {
        HStack(alignment: .bottom, spacing: 10) {
            TextField("Message the agent…", text: $draft, axis: .vertical)
                .textFieldStyle(.plain)
                .lineLimit(1...6)
                .padding(.horizontal, 12)
                .padding(.vertical, 9)
                .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                .overlay {
                    RoundedRectangle(cornerRadius: 10, style: .continuous)
                        .stroke(.separator.opacity(0.55), lineWidth: 1)
                }
                .disabled(chat.busy)
                .onSubmit(send)

            Button {
                if chat.busy { interrupt() } else { send() }
            } label: {
                Image(systemName: chat.busy ? "stop.fill" : "arrow.up")
                    .font(.system(size: 13, weight: .bold))
                    .frame(width: 28, height: 28)
            }
            .buttonStyle(.borderedProminent)
            .tint(chat.busy ? .red : .accentColor)
            .disabled(!chat.busy && draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            .help(chat.busy ? "Interrupt agent" : "Send message")
        }
        .padding(14)
    }

    private var scrollToken: String {
        "\(chat.messages.count):\(chat.messages.last?.text.count ?? 0):\(chat.pendingPermission?.requestID ?? "")"
    }

    private func send() {
        let text = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty, !chat.busy else { return }
        draft = ""
        model.sendAgentMessage(paneID: pane.id, text: text)
    }

    private func interrupt() { model.interruptAgent(pane.id) }
}

private struct EmptyChatView: View {
    var body: some View {
        VStack(spacing: 12) {
            Image(systemName: "sparkles")
                .font(.system(size: 28, weight: .light))
                .foregroundStyle(.secondary)
            Text("Start a conversation")
                .font(.title3.weight(.medium))
            Text("This agent runs in the Sgian daemon and will keep its session when this window closes.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
        }
        .frame(maxWidth: 420)
        .frame(maxWidth: .infinity, minHeight: 260)
    }
}

private struct ChatMessageView: View {
    let message: ChatMessage

    var body: some View {
        switch message.kind {
        case .user:
            HStack {
                Spacer(minLength: 100)
                Text(message.text)
                    .textSelection(.enabled)
                    .padding(.horizontal, 13)
                    .padding(.vertical, 9)
                    .background(Color.accentColor.opacity(0.16), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
            }
        case .assistant:
            HStack(alignment: .top, spacing: 10) {
                DaggerMark(size: 17).foregroundStyle(Color.accentColor)
                Text(message.text.isEmpty ? "…" : message.text)
                    .textSelection(.enabled)
                    .lineSpacing(3)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        case .tool:
            DisclosureGroup {
                VStack(alignment: .leading, spacing: 10) {
                    if let detail = message.detail, !detail.isEmpty {
                        Text(detail).font(.caption.monospaced()).textSelection(.enabled)
                    }
                    if !message.text.isEmpty {
                        Divider()
                        Text(message.text)
                            .font(.caption.monospaced())
                            .foregroundStyle(message.isError ? Color.red : Color.secondary)
                            .textSelection(.enabled)
                    }
                }
                .padding(.top, 8)
            } label: {
                Label(message.title ?? "Tool", systemImage: "wrench.and.screwdriver")
                    .font(.callout.weight(.medium))
            }
            .padding(11)
            .background(.quaternary.opacity(0.35), in: RoundedRectangle(cornerRadius: 9, style: .continuous))
        case .toolResult:
            Label {
                Text(message.text).font(.caption.monospaced()).textSelection(.enabled)
            } icon: {
                Image(systemName: message.isError ? "exclamationmark.triangle" : "checkmark.circle")
            }
            .foregroundStyle(message.isError ? Color.red : Color.secondary)
        case .error:
            Label(message.text, systemImage: "exclamationmark.triangle.fill")
                .foregroundStyle(.red)
                .padding(10)
                .background(Color.red.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
        case .historyElided:
            Text(message.text)
                .font(.caption)
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, alignment: .center)
        }
    }
}

private struct PermissionCard: View {
    @ObservedObject var model: WorkspaceModel
    let paneID: String
    let permission: PendingPermission

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Label("Permission requested", systemImage: "lock.shield")
                .font(.headline)
            Text(permission.toolName).font(.callout.weight(.semibold))
            if let input = permission.input, input != .null {
                Text(input.prettyPrinted)
                    .font(.caption.monospaced())
                    .textSelection(.enabled)
                    .padding(9)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(.black.opacity(0.08), in: RoundedRectangle(cornerRadius: 7))
            }
            HStack {
                Button("Deny", role: .destructive) {
                    model.answerPermission(
                        paneID: paneID,
                        requestID: permission.requestID,
                        allow: false,
                        message: "Denied by user"
                    )
                }
                Spacer()
                Button("Allow") {
                    model.answerPermission(paneID: paneID, requestID: permission.requestID, allow: true)
                }
                .buttonStyle(.borderedProminent)
            }
        }
        .padding(14)
        .background(Color.orange.opacity(0.1), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .stroke(Color.orange.opacity(0.35))
        }
    }
}
