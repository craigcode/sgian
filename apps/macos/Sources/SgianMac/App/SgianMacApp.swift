import SwiftUI

@main
struct SgianMacApp: App {
    @NSApplicationDelegateAdaptor(NativeApplicationDelegate.self) private var lifecycle
    @Environment(\.openWindow) private var openWindow
    @StateObject private var windows = WorkspaceWindows()
    @StateObject private var updater = NativeUpdater()
    private var model: WorkspaceModel { windows.activeModel }

    var body: some Scene {
        let _ = configureLifecycle()
        WindowGroup("Sgian", id: NativeApplicationDelegate.workspaceID) {
            WorkspaceWindow(windows: windows)
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
                Button("Take Keyboard") { model.takeLease() }
                    .keyboardShortcut("t", modifiers: [.command, .shift])
                Button("Release Keyboard…") { model.openReleaseDialog() }
                    .keyboardShortcut("l", modifiers: [.command, .shift])
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

    private func configureLifecycle() {
        let action = openWindow
        lifecycle.openWorkspace = { action(id: NativeApplicationDelegate.workspaceID) }
    }
}

@MainActor
final class NativeApplicationDelegate: NSObject, NSApplicationDelegate {
    static let workspaceID = "workspace"
    var openWorkspace: (() -> Void)?

    private func hasWorkspaceWindow(_ app: NSApplication) -> Bool {
        app.windows.contains { $0.identifier?.rawValue.hasPrefix(Self.workspaceID) == true }
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        // An older development build may have saved an obsolete SwiftUI scene
        // identifier. Let normal restoration finish, then recover an empty app.
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            if !self.hasWorkspaceWindow(.shared) { self.openWorkspace?() }
        }
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows: Bool) -> Bool {
        if !hasWorkspaceWindow(sender) { openWorkspace?(); return false }
        return true
    }
}

@MainActor
private final class WorkspaceWindows: ObservableObject {
    @Published var activeModel = WorkspaceModel()
}

private struct WorkspaceWindow: View {
    @ObservedObject var windows: WorkspaceWindows
    @StateObject private var model = WorkspaceModel()

    var body: some View {
        RootView(model: model)
            .background(WindowFocusObserver { windows.activeModel = model }.frame(width: 0, height: 0))
            .onAppear { windows.activeModel = model }
    }
}

// Each window owns its NSViews. Commands and Settings follow the key window.
private struct WindowFocusObserver: NSViewRepresentable {
    let focused: () -> Void
    func makeNSView(context: Context) -> ObserverView { ObserverView(focused: focused) }
    func updateNSView(_ nsView: ObserverView, context: Context) { nsView.focused = focused }

    final class ObserverView: NSView {
        var focused: () -> Void
        private var observer: NSObjectProtocol?
        init(focused: @escaping () -> Void) { self.focused = focused; super.init(frame: .zero) }
        required init?(coder: NSCoder) { fatalError("init(coder:) is not supported") }
        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            if let observer { NotificationCenter.default.removeObserver(observer) }
            guard let window else { observer = nil; return }
            observer = NotificationCenter.default.addObserver(forName: NSWindow.didBecomeKeyNotification,
                object: window, queue: .main) { [weak self] _ in
                    MainActor.assumeIsolated { self?.focused() }
                }
        }
        deinit { if let observer { NotificationCenter.default.removeObserver(observer) } }
    }
}
