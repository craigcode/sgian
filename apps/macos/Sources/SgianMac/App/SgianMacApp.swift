import SwiftUI

@main
struct SgianMacApp: App {
    @StateObject private var model = WorkspaceModel()

    var body: some Scene {
        WindowGroup("Sgian") {
            RootView(model: model)
        }
        .defaultSize(width: 1220, height: 780)
        .windowToolbarStyle(.unified(showsTitle: false))
        .commands {
            CommandGroup(replacing: .newItem) {
                Button("New Terminal") { model.createShell() }
                    .keyboardShortcut("t", modifiers: .command)
                Button("New Agent") { model.createAgent(backend: defaultAgentBackend) }
                    .keyboardShortcut("a", modifiers: [.command, .shift])
            }
            CommandMenu("Pane") {
                Button("Restart Pane") {
                    if let paneID = model.selectedPaneID { model.restart(paneID) }
                }
                .keyboardShortcut("r", modifiers: [.command, .shift])
                Button("Clear Scrollback") { model.clearSelectedTerminal() }
                    .keyboardShortcut("k", modifiers: .command)
                Divider()
                Button("Close Pane") {
                    if let paneID = model.selectedPaneID { model.close(paneID) }
                }
                .keyboardShortcut("w", modifiers: [.command, .shift])
            }
            CommandMenu("Workspace") {
                Button("Open Workspace…", action: model.chooseWorkspace)
                    .keyboardShortcut("o", modifiers: [.command, .shift])
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
