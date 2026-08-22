import SwiftUI

struct RootView: View {
    @ObservedObject var model: WorkspaceModel

    var body: some View {
        NavigationSplitView {
            SidebarView(model: model)
                .navigationSplitViewColumnWidth(min: 190, ideal: 235, max: 330)
        } detail: {
            detail
        }
        .frame(minWidth: 900, minHeight: 580)
        .toolbar { toolbar }
        .task { model.start() }
        .alert("Sgian", isPresented: errorPresented) {
            Button("OK") { model.errorMessage = nil }
        } message: {
            Text(model.errorMessage ?? "Unknown error")
        }
    }

    @ViewBuilder
    private var detail: some View {
        if let pane = model.selectedPane {
            VStack(spacing: 0) {
                PaneHeader(model: model, pane: pane)
                Divider()
                if pane.kind == .shell, let terminal = model.terminals[pane.id] {
                    TerminalSurfaceView(surface: terminal)
                        .padding(.horizontal, 8)
                        .padding(.vertical, 6)
                        .background(Color(nsColor: terminal.view.nativeBackgroundColor))
                        .overlay {
                            Rectangle()
                                .strokeBorder(Color(nsColor: .separatorColor), lineWidth: 1)
                                .accessibilityHidden(true)
                        }
                } else if pane.kind == .agent {
                    AgentChatView(model: model, pane: pane)
                } else {
                    ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity)
                }
            }
        } else {
            EmptyWorkspaceView(model: model)
        }
    }

    @ToolbarContentBuilder
    private var toolbar: some ToolbarContent {
        ToolbarItemGroup(placement: .primaryAction) {
            Button(action: { model.createShell() }) {
                Label("New Terminal", systemImage: "plus")
            }
            .help("New terminal (⌘T)")

            Menu {
                Button("Claude") { model.createAgent(backend: .claude) }
                Button("Factory Droid") { model.createAgent(backend: .droid) }
            } label: {
                Label("New Agent", systemImage: "sparkles")
            }
            .help("New agent")
        }
    }

    private var errorPresented: Binding<Bool> {
        Binding(get: { model.errorMessage != nil }, set: { if !$0 { model.errorMessage = nil } })
    }
}

private struct PaneHeader: View {
    @ObservedObject var model: WorkspaceModel
    let pane: Pane

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: pane.kind == .shell ? "terminal" : "bubble.left.and.bubble.right")
                .foregroundStyle(.secondary)
            Text(pane.title).font(.headline)
            if pane.kind == .agent, let spec = model.agentSpecs[pane.id] {
                Text([spec.backend.displayName, spec.model].compactMap { $0 }.joined(separator: " · "))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            if model.paneStates[pane.id] == .ended {
                Button("Restart", systemImage: "arrow.clockwise") { model.restart(pane.id) }
                    .buttonStyle(.bordered)
                    .controlSize(.small)
            }
        }
        .padding(.horizontal, 14)
        .frame(height: 43)
        .background(.bar)
    }
}

private struct EmptyWorkspaceView: View {
    @ObservedObject var model: WorkspaceModel

    var body: some View {
        VStack(spacing: 16) {
            DaggerMark(size: 52).foregroundStyle(Color.accentColor)
            Text(statusTitle).font(.title2.weight(.semibold))
            Text(statusMessage)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 420)
            if model.status == .connected {
                HStack {
                    Button("New Terminal") { model.createShell() }
                    Button("New Claude Agent") { model.createAgent(backend: .claude) }
                }
            } else if case .failed = model.status {
                Button("Reconnect") { model.connect(to: model.workspaceURL) }
                    .buttonStyle(.borderedProminent)
            } else {
                ProgressView()
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var statusTitle: String {
        switch model.status {
        case .connecting: "Connecting to Sgian"
        case .connected: "Your workspace is ready"
        case .failed: "Couldn’t connect"
        case .disconnected: "Sgian"
        }
    }

    private var statusMessage: String {
        switch model.status {
        case .connecting: "Attaching to the session daemon for \(model.workspaceURL.path)"
        case .connected: "Create a terminal or agent conversation to get started."
        case let .failed(message): message
        case .disconnected: "A native workspace for terminals and coding agents."
        }
    }
}
