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
                if !model.savedMachines.isEmpty {
                    Section("Machines") {
                        if model.activeMachine != nil {
                            Button(action: model.connectToLocalWorkspace) {
                                HStack(spacing: 9) {
                                    Image(systemName: "laptopcomputer")
                                        .frame(width: 17)
                                        .foregroundStyle(.secondary)
                                        .accessibilityHidden(true)
                                    VStack(alignment: .leading, spacing: 1) {
                                        Text(model.localWorkspaceURL.lastPathComponent)
                                            .lineLimit(1)
                                        Text("Local · \(model.localWorkspaceURL.deletingLastPathComponent().path)")
                                            .font(.caption2)
                                            .foregroundStyle(.secondary)
                                            .lineLimit(1)
                                            .truncationMode(.middle)
                                    }
                                    Spacer(minLength: 4)
                                }
                                .padding(.vertical, 2)
                                .contentShape(Rectangle())
                            }
                            .buttonStyle(.plain)
                            .help("Switch back to local workspace")
                            .accessibilityLabel("Switch to local workspace \(model.localWorkspaceURL.lastPathComponent)")
                        }
                        ForEach(model.savedMachines) { machine in
                            MachineRow(
                                machine: machine,
                                isActive: model.activeMachine?.name == machine.name,
                                summary: model.machineSummaries[machine.name],
                                activeStatus: model.activeMachine?.name == machine.name ? model.status : nil,
                                action: { model.connect(toMachine: machine) }
                            )
                        }
                    }
                }
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
                                rollup: model.rollupText(for: group),
                                limit: model.limitText(for: group),
                                notes: model.noteLines(for: group)
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
            outputWarning: model.outputWarning(for: pane.id),
            usage: model.usage(for: pane.id)
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
        let footerSummary = SidebarAccessibility.workspaceFooterLabel(
            workspaceURL: model.workspaceURL,
            activeMachine: model.activeMachine,
            status: model.status
        )
        let footerAction = model.activeMachine == nil ? "Change workspace" : "Open a local workspace"
        return Button(action: model.chooseWorkspace) {
            HStack(spacing: 9) {
                Image(systemName: model.activeMachine == nil ? "folder" : "network")
                    .foregroundStyle(model.activeMachine == nil ? Color.secondary : Color.accentColor)
                    .accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 1) {
                    Text(model.activeMachine?.name ?? model.workspaceURL.lastPathComponent)
                        .font(.callout.weight(.medium))
                        .lineLimit(1)
                    Text(model.activeMachine?.displaySubtitle ?? model.workspaceURL.deletingLastPathComponent().path)
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
        .help(footerAction)
        .accessibilityLabel(footerSummary)
        .accessibilityHint(footerAction)
    }
}

enum SidebarAccessibility {
    static func projectHeaderLabel(
        title: String,
        goal: String?,
        rollup: String,
        limit: String? = nil,
        notes: [String] = []
    ) -> String {
        var parts = [title]
        if let goal, !goal.isEmpty { parts.append(goal) }
        if !rollup.isEmpty { parts.append(rollup) }
        if let limit, !limit.isEmpty { parts.append(limit) }
        parts.append(contentsOf: notes.filter { !$0.isEmpty })
        return parts.joined(separator: ". ")
    }

    static func paneRowLabel(
        pane: Pane,
        runtime: PaneRuntimeState,
        attention: AgentAttention?,
        spec: AgentPaneSpec?,
        leaseHolder: String? = nil,
        ownLease: Bool = false,
        unattended: Bool = false,
        outputWarning: OutputTricks? = nil,
        usage: AgentUsage? = nil
    ) -> String {
        var parts = [pane.title]
        if pane.kind == .shell {
            parts.append("Terminal")
        } else {
            let identity = [spec?.backend.displayName, spec?.model]
                .compactMap { $0 }
                .filter { !$0.isEmpty }
                .joined(separator: " · ")
            parts.append(identity.isEmpty ? "Agent" : identity)
        }
        if runtime == .ended {
            parts.append("Session ended")
        } else if attention == .needsInput {
            parts.append("Needs input")
        } else if attention == .working {
            parts.append("Working")
        }
        if unattended {
            parts.append("Agent runs tools without approval")
        }
        if let outputWarning {
            parts.append("Output hid something: \(outputWarning.summary)")
        }
        if let leaseHolder {
            parts.append(ownLease ? "You hold the keyboard" : "Keyboard held by \(leaseHolder)")
        }
        if let usage {
            let summary = usage.summary()
            if !summary.isEmpty { parts.append(summary) }
        }
        return parts.joined(separator: ", ")
    }

    static func machineRowLabel(
        machine: SavedMachine,
        isActive: Bool,
        summary: MachineSummary?,
        activeStatus: ConnectionStatus?
    ) -> String {
        var parts = [machine.name]
        if isActive { parts.append("Active") }
        parts.append(summary?.badgeText ?? machine.displaySubtitle)
        if let activeStatus {
            parts.append(activeStatus.label)
        } else if let summary, summary.connected {
            if summary.needsInputCount > 0 {
                parts.append("Needs input")
            } else if summary.workingCount > 0 {
                parts.append("Working")
            } else {
                parts.append("Tunnel connected")
            }
        }
        return parts.joined(separator: ", ")
    }

    static func workspaceFooterLabel(
        workspaceURL: URL,
        activeMachine: SavedMachine?,
        status: ConnectionStatus
    ) -> String {
        if let activeMachine {
            return "Remote workspace \(activeMachine.name), \(activeMachine.displaySubtitle), \(status.label)"
        }
        return "Local workspace \(workspaceURL.lastPathComponent), \(status.label)"
    }
}

private struct MachineRow: View {
    let machine: SavedMachine
    let isActive: Bool
    let summary: MachineSummary?
    let activeStatus: ConnectionStatus?
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 9) {
                Image(systemName: "network")
                    .frame(width: 17)
                    .foregroundStyle(isActive ? Color.accentColor : Color.secondary)
                    .accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 1) {
                    Text(machine.name)
                        .fontWeight(isActive ? .semibold : .regular)
                        .lineLimit(1)
                    Text(summary?.badgeText ?? machine.displaySubtitle)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                Spacer(minLength: 4)
                if let activeStatus {
                    ConnectionDot(status: activeStatus)
                } else if let summary, summary.connected {
                    if summary.needsInputCount > 0 {
                        Circle().fill(.orange).frame(width: 7, height: 7).help("Needs input")
                    } else if summary.workingCount > 0 {
                        ProgressView().controlSize(.mini).help("Working")
                    } else {
                        Circle().fill(.green).frame(width: 7, height: 7).help("Tunnel connected")
                    }
                }
            }
            .padding(.vertical, 2)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help("Connect to \(machine.name) (\(machine.displaySubtitle))")
        .accessibilityLabel(
            SidebarAccessibility.machineRowLabel(
                machine: machine,
                isActive: isActive,
                summary: summary,
                activeStatus: activeStatus
            )
        )
    }
}

private struct ProjectHeader: View {
    let title: String
    let goal: String?
    let rollup: String
    var limit: String? = nil
    /// Shared context notes: a summary line then the newest few, one each.
    var notes: [String] = []

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
            if let limit {
                Text(limit)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            ForEach(Array(notes.enumerated()), id: \.offset) { index, line in
                Text(line)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .padding(.leading, index == 0 ? 0 : 8)
            }
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel(
            SidebarAccessibility.projectHeaderLabel(
                title: title,
                goal: goal,
                rollup: rollup,
                limit: limit,
                notes: notes
            )
        )
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
    var usage: AgentUsage? = nil

    var body: some View {
        HStack(spacing: 9) {
            Image(systemName: pane.kind == .shell ? "terminal" : "bubble.left.and.bubble.right")
                .frame(width: 17)
                .foregroundStyle(iconColor)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                Text(pane.title)
                    .lineLimit(1)
                if pane.kind == .agent {
                    Text([spec?.backend.displayName, spec?.model].compactMap { $0 }.joined(separator: " · "))
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                if let usage {
                    Text(usage.summary())
                        .font(.caption2)
                        .foregroundStyle(usage.isHot ? Color.orange : Color.secondary)
                        .lineLimit(1)
                        .help("From the session's status line (sgian ctl statusline)")
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
        .accessibilityElement(children: .combine)
        .accessibilityLabel(
            SidebarAccessibility.paneRowLabel(
                pane: pane,
                runtime: runtime,
                attention: attention,
                spec: spec,
                leaseHolder: leaseHolder,
                ownLease: ownLease,
                unattended: unattended,
                outputWarning: outputWarning,
                usage: usage
            )
        )
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
