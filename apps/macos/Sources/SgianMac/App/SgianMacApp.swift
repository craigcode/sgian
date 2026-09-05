import SwiftUI

@main
struct SgianMacApp: App {
    @StateObject private var model = WorkspaceModel()
    @StateObject private var updater = NativeUpdater()

    var body: some Scene {
        WindowGroup("Sgian") {
            RootView(model: model)
        }
        .defaultSize(width: 1220, height: 780)
        .windowToolbarStyle(.unified(showsTitle: false))
        .commands {
            CommandGroup(after: .appInfo) {
                Button("Check for Updates…", action: updater.check).disabled(!updater.canCheck)
            }
            CommandGroup(replacing: .newItem) {
                Button("New Terminal") { model.createShell() }
                    .keyboardShortcut("t", modifiers: .command)
                Button("New Agent") { model.createAgent(backend: defaultAgentBackend) }
                    .keyboardShortcut("a", modifiers: [.command, .shift])
            }
            CommandMenu("Pane") {
                Button("Split Right") { model.createShell(direction: "row") }
                    .keyboardShortcut("d", modifiers: .command)
                Button("Split Down") { model.createShell(direction: "column") }
                    .keyboardShortcut("d", modifiers: [.command, .shift])
                Button("Zoom Pane", action: model.toggleZoom)
                    .keyboardShortcut("z", modifiers: [.command, .shift])
                Button("Next Pane") { model.focusNext(1) }
                    .keyboardShortcut("]", modifiers: [.command, .shift])
                Button("Previous Pane") { model.focusNext(-1) }
                    .keyboardShortcut("[", modifiers: [.command, .shift])
                Button("Find in Terminal") { model.showingSearch = true }
                    .keyboardShortcut("f", modifiers: [.command, .shift])
                Button("Command Palette") { model.showingCommands = true }
                    .keyboardShortcut("p", modifiers: [.command, .shift])
                Divider()
                Button("Restart Pane") {
                    if let paneID = model.selectedPaneID { model.restart(paneID) }
                }
                .keyboardShortcut("r", modifiers: [.command, .shift])
                Button("Clear Scrollback") { model.clearSelectedTerminal() }
                    .keyboardShortcut("k", modifiers: .command)
                Divider()
                Button("Close Pane") {
                    model.requestClose()
                }
                .keyboardShortcut("w", modifiers: [.command, .shift])
            }
            CommandMenu("Workspace") {
                Button("Open Workspace…", action: model.chooseWorkspace)
                    .keyboardShortcut("o", modifiers: [.command, .shift])
                Menu("Recent Workspaces") {
                    ForEach(model.recentWorkspaces, id: \.self) { path in
                        Button(path) { model.connect(to: URL(fileURLWithPath: path)) }
                    }
                }
                Button("Reconnect") { model.connect(to: model.workspaceURL) }
            }
        }

        Settings {
            SettingsView(model: model)
        }
    }

    private var defaultAgentBackend: AgentBackend {
        AgentBackend(rawValue: UserDefaults.standard.string(forKey: "defaultAgentBackend") ?? "claude") ?? .claude
    }
}
