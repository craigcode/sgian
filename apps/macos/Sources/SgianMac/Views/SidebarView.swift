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
                if model.projects.isEmpty {
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
                } else {
                    // The project board: one section per project with its
                    // attention roll-up, unassigned panes last.
                    ForEach(model.projectGroups()) { group in
                        Section {
                            ForEach(group.panes) { pane in row(pane) }
                        } header: {
                            ProjectHeader(
                                title: group.title,
                                goal: group.goal,
                                rollup: model.rollupText(for: group)
                            )
                        }
                    }
                }
            }
            .listStyle(.sidebar)
            .scrollContentBackground(.hidden)

            Divider()
            workspaceFooter
        }
        .background(.white)
        .environment(\.colorScheme, .light)
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
            attention: model.attention(for: pane.id),
            spec: model.agentSpecs[pane.id],
            leaseHolder: model.lease(for: pane.id)?.holder,
            ownLease: model.isOwnLease(model.lease(for: pane.id)),
            unattended: model.agentStates[pane.id]?.isUnattended ?? false,
            outputWarning: model.outputWarning(for: pane.id)
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

private struct ProjectHeader: View {
    let title: String
    let goal: String?
    let rollup: String

    var body: some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(title)
            if let goal {
                Text(goal)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Text(rollup)
                .font(.caption2)
                .foregroundStyle(.secondary)
                .lineLimit(2)
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(title). \(rollup)")
    }
}

private struct PaneRow: View {
    let pane: Pane
    let runtime: PaneRuntimeState
    let attention: AgentAttention?
    let spec: AgentPaneSpec?
    var leaseHolder: String? = nil
    var ownLease = false
    var unattended = false
    var outputWarning: OutputTricks? = nil

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
            if unattended {
                Image(systemName: "exclamationmark.shield.fill")
                    .font(.caption2)
                    .foregroundStyle(.orange)
                    .help("Agent runs tools without approval")
            }
            if let outputWarning {
                Image(systemName: "eye.slash.fill")
                    .font(.caption2)
                    .foregroundStyle(.orange)
                    .help("Output hid something: \(outputWarning.summary)")
                    .accessibilityLabel("Output hid something: \(outputWarning.summary)")
            }
            if let leaseHolder {
                Image(systemName: "keyboard")
                    .font(.caption2)
                    .foregroundStyle(ownLease ? Color.green : Color.orange)
                    .help(ownLease ? "You hold the keyboard" : "Keyboard held by \(leaseHolder)")
            }
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
