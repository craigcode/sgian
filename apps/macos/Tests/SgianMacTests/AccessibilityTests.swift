import AppKit
import Foundation
import Testing
@testable import SgianMac

@Test @MainActor func terminalSurfacePublishesScreenTextAndCoalescesValueChangedNotifications() async throws {
    let surface = TerminalSurface(id: "pane-1", fontSize: 13, columns: 40, rows: 6)
    #expect(surface.view.isAccessibilityElement())
    #expect(surface.view.accessibilityRole() == .textArea)
    #expect(surface.view.accessibilityLabel() == "Terminal pane-1")
    #expect((surface.view.accessibilityValue() as? String) == "")
    #expect(surface.accessibilityValueChangeCount == 0)

    surface.setTitle("build-shell")
    #expect(surface.view.accessibilityLabel() == "Terminal build-shell")

    // Burst output within the 150 ms coalescing window publishes once.
    surface.feed("hello world\r\n")
    surface.feed("prompt> ready")
    try await Task.sleep(for: .milliseconds(240))

    let published = try #require(surface.view.accessibilityValue() as? String)
    #expect(published.hasPrefix("hello world\nprompt> ready"))
    #expect(surface.accessibilityValueChangeCount == 1)

    // Re-running refresh when the visible buffer has not changed does not spam VoiceOver.
    surface.refreshAccessibilityValue()
    #expect(surface.accessibilityValueChangeCount == 1)
}

@Test func sidebarAccessibilityIncludesGoalAndAllPaneBadges() {
    let headerWithGoal = SidebarAccessibility.projectHeaderLabel(
        title: "release-prep",
        goal: "Ship 0.1.0",
        rollup: "2 panes · 1 input",
        limit: "5h 42%",
        notes: ["1 note", "Audit complete — craig@mac · 2026-11-12"]
    )
    #expect(headerWithGoal == "release-prep. Ship 0.1.0. 2 panes · 1 input. 5h 42%. 1 note. Audit complete — craig@mac · 2026-11-12")

    let headerWithoutGoal = SidebarAccessibility.projectHeaderLabel(
        title: "unassigned",
        goal: nil,
        rollup: "1 pane"
    )
    #expect(headerWithoutGoal == "unassigned. 1 pane")

    let agentPane = Pane(id: "agent-1", title: "reviewer", kind: .agent, createdAtMs: 1)
    let agentLabel = SidebarAccessibility.paneRowLabel(
        pane: agentPane,
        runtime: .live,
        attention: .needsInput,
        spec: AgentPaneSpec(backend: .claude, model: "opus"),
        leaseHolder: "alice@mac",
        ownLease: false,
        unattended: true,
        outputWarning: OutputTricks(conceal: 2, clipboard: 1),
        usage: AgentUsage(model: "Opus", contextUsedPercentage: 45, updatedAtMs: 1)
    )
    #expect(agentLabel == "reviewer, Claude · opus, Needs input, Agent runs tools without approval, Output hid something: 2 concealed text, 1 clipboard writes, Keyboard held by alice@mac, Opus · 45% context")

    let endedShell = Pane(id: "shell-1", title: "zsh", kind: .shell, createdAtMs: 2)
    let endedLabel = SidebarAccessibility.paneRowLabel(
        pane: endedShell,
        runtime: .ended,
        attention: .working,
        spec: nil,
        leaseHolder: "me@mac",
        ownLease: true
    )
    #expect(endedLabel == "zsh, Terminal, Session ended, You hold the keyboard")
}

@Test func sidebarAccessibilityDescribesMachinesAndWorkspaceFooter() {
    let machine = SavedMachine(
        name: "gpu-box",
        transport: "ssh",
        host: "dev@gpu.internal",
        port: 2222,
        workspace: "/srv/sgian",
        target: nil,
        tokenFile: nil
    )
    let idleTunnelLabel = SidebarAccessibility.machineRowLabel(
        machine: machine,
        isActive: false,
        summary: MachineSummary(connected: true, paneCount: 2, needsInputCount: 1, workingCount: 0),
        activeStatus: nil
    )
    #expect(idleTunnelLabel == "gpu-box, 1 input, Needs input")

    let activeLabel = SidebarAccessibility.machineRowLabel(
        machine: machine,
        isActive: true,
        summary: nil,
        activeStatus: .connected
    )
    #expect(activeLabel == "gpu-box, Active, dev@gpu.internal:2222 · /srv/sgian, Connected")

    let localFooter = SidebarAccessibility.workspaceFooterLabel(
        workspaceURL: URL(fileURLWithPath: "/Users/op/Data/sgian"),
        activeMachine: nil,
        status: .connected
    )
    #expect(localFooter == "Local workspace sgian, Connected")

    let remoteFooter = SidebarAccessibility.workspaceFooterLabel(
        workspaceURL: URL(fileURLWithPath: "/srv/sgian"),
        activeMachine: machine,
        status: .connecting
    )
    #expect(remoteFooter == "Remote workspace gpu-box, dev@gpu.internal:2222 · /srv/sgian, Connecting")
}
