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
        .sheet(isPresented: $model.showingCommands) { NativeCommandPalette(model: model) }
        .confirmationDialog("Close pane?", isPresented: Binding(
            get: { model.panePendingClose != nil },
            set: { if !$0 { model.panePendingClose = nil } }
        ), titleVisibility: .visible) {
            if let pane = model.panePendingClose {
                Button("Close \(pane.title)", role: .destructive) { model.close(pane.id); model.panePendingClose = nil }
                Button("Cancel", role: .cancel) { model.panePendingClose = nil }
            }
        } message: { Text("This stops the pane’s running process. Closing the window leaves sessions running.") }
        .alert("Sgian", isPresented: errorPresented) {
            Button("OK") { model.errorMessage = nil }
        } message: {
            Text(model.errorMessage ?? "Unknown error")
        }
    }

    @ViewBuilder
    private var detail: some View {
        VStack(spacing: 0) {
            if model.showingSearch { TerminalSearchBar(model: model) }
            if let layout = model.layout {
                NativePaneTree(model: model, node: model.zoomed && model.selectedPaneID != nil
                    ? .leaf(model.selectedPaneID!) : layout)
            } else {
                EmptyWorkspaceView(model: model)
            }
        }
    }

    @ToolbarContentBuilder
    private var toolbar: some ToolbarContent {
        ToolbarItemGroup(placement: .primaryAction) {
            Button(action: { model.createShell() }) {
                Label("New Terminal", systemImage: "plus")
            }
            .help("New terminal (⌘T)")

            Button { model.createShell(direction: "column") } label: {
                Label("Split Down", systemImage: "rectangle.split.1x2")
            }
            Button { model.toggleZoom() } label: {
                Label(model.zoomed ? "Show All Panes" : "Zoom Pane", systemImage: "arrow.up.left.and.arrow.down.right")
            }
            Button { model.showingCommands = true } label: { Label("Commands", systemImage: "command") }
            Menu {
                Button("Claude") { model.createAgent(backend: .claude) }
                Button("Factory Droid") { model.createAgent(backend: .droid) }
                if !model.profiles.isEmpty {
                    Divider()
                    ForEach(model.profiles, id: \.prettyPrinted) { profile in
                        Button(profile["name"]?.stringValue ?? "Profile") { model.createProfile(profile) }
                    }
                }
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

struct PaneHeader: View {
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
            Button { model.select(pane.id); model.toggleZoom() } label: {
                Image(systemName: model.zoomed ? "arrow.down.right.and.arrow.up.left" : "arrow.up.left.and.arrow.down.right")
            }.buttonStyle(.plain).help("Zoom pane")
            Button { model.requestClose(pane) } label: { Image(systemName: "xmark") }
                .buttonStyle(.plain).help("Close pane")
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
