import SwiftUI

struct SidebarView: View {
    @ObservedObject var model: WorkspaceModel
    @State private var paneToRename: Pane?
    @State private var proposedTitle = ""

    private var selection: Binding<String?> {
        Binding(get: { model.selectedPaneID }, set: { model.select($0) })
    }

    var body: some View {
        VStack(spacing: 0) {
            List(selection: selection) {
                if !shells.isEmpty {
                    Section("Terminals") {
                        ForEach(shells) { pane in row(pane) }
                    }
                }
                if !agents.isEmpty {
                    Section("Agents") {
                        ForEach(agents) { pane in row(pane) }
                    }
                }
            }
            .listStyle(.sidebar)

            Divider()
            workspaceFooter
        }
        .navigationTitle("Sgian")
        .toolbar {
            ToolbarItem(placement: .navigation) {
                DaggerMark(size: 18)
                    .foregroundStyle(Color.accentColor)
                    .accessibilityLabel("Sgian")
            }
        }
        .sheet(item: $paneToRename) { pane in
            RenamePaneSheet(
                pane: pane,
                title: $proposedTitle,
                onCancel: { paneToRename = nil },
                onSave: {
                    model.rename(pane.id, to: proposedTitle)
                    paneToRename = nil
                }
            )
        }
    }

    private var shells: [Pane] { model.panes.filter { $0.kind == .shell } }
    private var agents: [Pane] { model.panes.filter { $0.kind == .agent } }

    private func row(_ pane: Pane) -> some View {
        PaneRow(
            pane: pane,
            runtime: model.paneStates[pane.id] ?? .live,
            attention: model.agentStates[pane.id]?.attention,
            spec: model.agentSpecs[pane.id]
        )
        .tag(pane.id)
        .contextMenu {
            Button("Rename…") {
                proposedTitle = pane.title
                paneToRename = pane
            }
            Button("Restart") { model.restart(pane.id) }
            Divider()
            Button("Close Pane", role: .destructive) { model.requestClose(pane) }
        }
    }

    private var workspaceFooter: some View {
        Button(action: model.chooseWorkspace) {
            HStack(spacing: 9) {
                Image(systemName: "folder")
                    .foregroundStyle(.secondary)
                VStack(alignment: .leading, spacing: 1) {
                    Text(model.workspaceURL.lastPathComponent)
                        .font(.callout.weight(.medium))
                        .lineLimit(1)
                    Text(model.workspaceURL.deletingLastPathComponent().path)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                Spacer(minLength: 0)
                ConnectionDot(status: model.status)
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 9)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help("Change workspace")
    }
}

private struct PaneRow: View {
    let pane: Pane
    let runtime: PaneRuntimeState
    let attention: AgentAttention?
    let spec: AgentPaneSpec?

    var body: some View {
        HStack(spacing: 9) {
            Image(systemName: pane.kind == .shell ? "terminal" : "bubble.left.and.bubble.right")
                .frame(width: 17)
                .foregroundStyle(iconColor)
            VStack(alignment: .leading, spacing: 2) {
                Text(pane.title)
                    .lineLimit(1)
                if pane.kind == .agent {
                    Text([spec?.backend.displayName, spec?.model].compactMap { $0 }.joined(separator: " · "))
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }
            Spacer(minLength: 4)
            if runtime == .ended {
                Image(systemName: "stop.circle.fill")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .help("Session ended")
            } else if attention == .needsInput {
                Circle().fill(.orange).frame(width: 7, height: 7).help("Needs input")
            } else if attention == .working {
                ProgressView().controlSize(.mini).help("Working")
            }
        }
        .padding(.vertical, 3)
    }

    private var iconColor: Color {
        if runtime == .ended { return .secondary }
        if attention == .needsInput { return .orange }
        return .accentColor
    }
}

private struct ConnectionDot: View {
    let status: ConnectionStatus

    var body: some View {
        Group {
            if status == .connecting {
                ProgressView().controlSize(.mini)
            } else {
                Circle()
                    .fill(status == .connected ? Color.green : Color.red)
                    .frame(width: 7, height: 7)
            }
        }
        .help(status.label)
    }
}

private struct RenamePaneSheet: View {
    let pane: Pane
    @Binding var title: String
    let onCancel: () -> Void
    let onSave: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            Text("Rename Pane").font(.title2.weight(.semibold))
            TextField("Pane name", text: $title)
                .textFieldStyle(.roundedBorder)
                .onSubmit(onSave)
            HStack {
                Spacer()
                Button("Cancel", action: onCancel)
                    .keyboardShortcut(.cancelAction)
                Button("Rename", action: onSave)
                    .keyboardShortcut(.defaultAction)
                    .disabled(title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(24)
        .frame(width: 360)
    }
}
