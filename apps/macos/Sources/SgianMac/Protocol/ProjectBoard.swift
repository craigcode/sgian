import Foundation

/// One section of the project board: a project (or `name == nil` for panes in
/// no project) and its member panes in project order.
struct ProjectGroup: Identifiable, Equatable {
    let name: String?
    let goal: String?
    let panes: [Pane]

    var id: String { name ?? "\u{0}unassigned" }
    var title: String { name ?? "No project" }
}

/// The attention roll-up for a set of panes: what one glance needs to tell.
/// Mirrors the daemon's `ProjectSummary` so `ctl project list` and the
/// sidebar agree.
struct ProjectRollup: Equatable {
    var panes = 0
    var live = 0
    var needsInput = 0
    var working = 0
    var idle = 0
    var unattended = 0
    var held = 0
    var holders: [String] = []
    var warnings = 0

    /// "3 panes · 1 needs input · 1 working · ⚠ 1 unattended · ⌨ alice":
    /// only the parts that are non-zero.
    var text: String {
        var parts = ["\(panes) pane\(panes == 1 ? "" : "s")"]
        if live != panes { parts.append("\(live) live") }
        if needsInput > 0 { parts.append("\(needsInput) needs input") }
        if working > 0 { parts.append("\(working) working") }
        if idle > 0 { parts.append("\(idle) idle") }
        if unattended > 0 { parts.append("⚠ \(unattended) unattended") }
        if warnings > 0 { parts.append("\(warnings) with output warnings") }
        if !holders.isEmpty { parts.append("⌨ " + holders.joined(separator: ", ")) }
        return parts.joined(separator: " · ")
    }
}

/// Pure grouping and roll-up for the project board; no model access so it is
/// testable with plain values.
enum ProjectBoard {
    /// What a project header shows for its shared context notes: a summary
    /// ("2 notes · 1 hid text") then the newest `limit` notes, one line each;
    /// empty when the project has none.
    static func noteLines(_ notes: [ProjectNote], limit: Int = 3) -> [String] {
        if notes.isEmpty { return [] }
        var lines = ["\(notes.count) note\(notes.count == 1 ? "" : "s")"]
        let guarded = notes.filter(\.guarded).count
        if guarded > 0 { lines[0] += " · \(guarded) hid text" }
        lines.append(contentsOf: notes.prefix(limit).map(\.line))
        return lines
    }

    /// Projects sorted by name, member panes in project order; panes in no
    /// project last under `name: nil` (omitted when every pane is assigned).
    /// A member id the client does not know is skipped: it closed between two
    /// events.
    static func group(panes: [Pane], projects: [String: Project]) -> [ProjectGroup] {
        let byID = Dictionary(panes.map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
        var assigned = Set<String>()
        var groups: [ProjectGroup] = []
        for name in projects.keys.sorted() {
            guard let project = projects[name] else { continue }
            var members: [Pane] = []
            for id in project.panes {
                guard let pane = byID[id], assigned.insert(id).inserted else { continue }
                members.append(pane)
            }
            groups.append(ProjectGroup(name: name, goal: project.goal, panes: members))
        }
        let rest = panes.filter { !assigned.contains($0.id) }
        if !rest.isEmpty || groups.isEmpty {
            groups.append(ProjectGroup(name: nil, goal: nil, panes: rest))
        }
        return groups
    }

    static func rollup(
        panes: [Pane],
        paneStates: [String: PaneRuntimeState],
        attention: (String) -> AgentAttention?,
        agentStates: [String: AgentPaneInfo],
        leases: [String: LeaseInfo],
        outputWarnings: [String: OutputTricks]
    ) -> ProjectRollup {
        var rollup = ProjectRollup(panes: panes.count)
        var holders = Set<String>()
        for pane in panes {
            if (paneStates[pane.id] ?? .live) == .live { rollup.live += 1 }
            switch attention(pane.id) {
            case .needsInput: rollup.needsInput += 1
            case .working: rollup.working += 1
            case .idle: rollup.idle += 1
            case nil: break
            }
            if agentStates[pane.id]?.isUnattended == true { rollup.unattended += 1 }
            if let holder = leases[pane.id]?.holder, !holder.isEmpty {
                rollup.held += 1
                holders.insert(holder)
            }
            if outputWarnings[pane.id] != nil { rollup.warnings += 1 }
        }
        rollup.holders = holders.sorted()
        return rollup
    }
}
