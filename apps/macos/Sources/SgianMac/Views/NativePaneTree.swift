import SwiftUI

struct NativePaneTree: View {
    @ObservedObject var model: WorkspaceModel
    let node: PaneLayout

    var body: some View { content }

    private var content: AnyView {
        switch node {
        case let .leaf(id):
            return AnyView(Group {
                if let pane = model.panes.first(where: { $0.id == id }) {
                    VStack(spacing: 0) {
                        PaneHeader(model: model, pane: pane)
                            .contentShape(Rectangle())
                            .onTapGesture { model.select(id) }
                        if pane.kind == .shell, let surface = model.terminals[id] {
                            TerminalSurfaceView(surface: surface)
                                .padding(4)
                                .background(Color(nsColor: surface.view.nativeBackgroundColor))
                        } else if pane.kind == .agent {
                            AgentChatView(model: model, pane: pane)
                        }
                    }
                    .overlay(Rectangle().strokeBorder(model.selectedPaneID == id ? Color.accentColor : Color.clear, lineWidth: 2).allowsHitTesting(false))
                    .simultaneousGesture(TapGesture().onEnded {
                        if model.selectedPaneID != id { model.select(id) }
                    })
                    .id(id)
                }
            })
        case let .split(id, direction, ratio, first, second):
            return AnyView(GeometryReader { geometry in
                let horizontal = direction == "row"
                let extent = max(1, (horizontal ? geometry.size.width : geometry.size.height) - 7)
                let firstSize = extent * ratio
                let secondSize = extent - firstSize
                let divider = NativeSplitDivider(horizontal: horizontal, ratio: ratio) { value in
                    model.resizeSplit(id, ratio: value)
                }.gesture(DragGesture(coordinateSpace: .named(id)).onChanged { event in
                    model.resizeSplit(id, ratio: (horizontal ? event.location.x : event.location.y) / extent)
                })
                Group {
                    if horizontal {
                        HStack(spacing: 0) {
                            NativePaneTree(model: model, node: first).frame(width: firstSize)
                            divider.frame(width: 7)
                            NativePaneTree(model: model, node: second).frame(width: secondSize)
                        }
                    } else {
                        VStack(spacing: 0) {
                            NativePaneTree(model: model, node: first).frame(height: firstSize)
                            divider.frame(height: 7)
                            NativePaneTree(model: model, node: second).frame(height: secondSize)
                        }
                    }
                }.coordinateSpace(name: id)
            })
        }
    }
}

private struct NativeSplitDivider: View {
    let horizontal: Bool
    let ratio: Double
    let resize: (Double) -> Void

    var body: some View {
        Rectangle().fill(Color(nsColor: .separatorColor).opacity(0.5))
            .contentShape(Rectangle())
            .accessibilityLabel(horizontal ? "Resize columns" : "Resize rows")
            .accessibilityValue("\(Int(ratio * 100)) percent")
            .accessibilityAdjustableAction { direction in
                resize(ratio + (direction == .increment ? 0.05 : -0.05))
            }
            .onHover { inside in
                if inside { (horizontal ? NSCursor.resizeLeftRight : NSCursor.resizeUpDown).push() }
                else { NSCursor.pop() }
            }
    }
}

struct TerminalSearchBar: View {
    @ObservedObject var model: WorkspaceModel
    @State private var query = ""
    @State private var result = ""
    @FocusState private var focused: Bool

    var body: some View {
        HStack {
            Image(systemName: "magnifyingglass")
            TextField("Find in active terminal", text: $query).focused($focused)
                .onSubmit { find(previous: false) }
            Text(result).foregroundStyle(.secondary).font(.caption)
            Button { find(previous: true) } label: { Image(systemName: "chevron.up") }.help("Previous match")
            Button { find(previous: false) } label: { Image(systemName: "chevron.down") }.help("Next match")
            Button { close() } label: { Image(systemName: "xmark") }.help("Close search")
        }
        .padding(8).background(.bar)
        .onAppear { focused = true }
        .onExitCommand { close() }
    }

    private func find(previous: Bool) {
        guard let id = model.selectedPaneID, let view = model.terminals[id]?.view else { result = "Select a terminal"; return }
        guard !query.isEmpty else { view.clearSearch(); result = ""; return }
        let found = previous ? view.findPrevious(query) : view.findNext(query)
        let summary = view.searchMatchSummary(query)
        result = found ? "\(summary.index) / \(summary.total)" : "No matches"
    }

    private func close() {
        if let id = model.selectedPaneID { model.terminals[id]?.view.clearSearch(); model.terminals[id]?.focus() }
        model.showingSearch = false
    }
}

struct NativeCommandPalette: View {
    @ObservedObject var model: WorkspaceModel
    @State private var query = ""
    @State private var selection: String?
    @FocusState private var focused: Bool

    private var commands: [(String, () -> Void)] {
        var items: [(String, () -> Void)] = [
            ("New terminal / Split right", { model.createShell() }),
            ("Split down", { model.createShell(direction: "column") }),
            ("New Claude agent", { model.createAgent(backend: .claude) }),
            ("New Factory Droid agent", { model.createAgent(backend: .droid) }),
            ("Zoom / Show all panes", { model.toggleZoom() }),
            ("Find in terminal", { model.showingSearch = true }),
            ("Open workspace…", { model.chooseWorkspace() }),
            ("Reconnect", { model.connect(to: model.workspaceURL) }),
            ("Close active pane…", { model.requestClose() }),
        ]
        items += model.panes.map { pane in ("Focus: \(pane.title) [\(pane.id)]", { model.select(pane.id) }) }
        return items.filter { query.isEmpty || $0.0.localizedCaseInsensitiveContains(query) }
    }

    var body: some View {
        VStack(spacing: 12) {
            TextField("Search commands and panes", text: $query).textFieldStyle(.roundedBorder)
                .focused($focused).onSubmit { run(selection ?? commands.first?.0) }
            List(selection: $selection) {
                ForEach(commands, id: \.0) { item in
                    Text(item.0).tag(item.0).onTapGesture(count: 2) { run(item.0) }
                }
            }
            HStack { Button("Cancel") { model.showingCommands = false }; Spacer(); Button("Run") { run(selection ?? commands.first?.0) }.keyboardShortcut(.defaultAction) }
        }.padding(20).frame(width: 520, height: 380)
            .onAppear { focused = true }
            .onExitCommand { model.showingCommands = false }
    }

    private func run(_ name: String?) {
        guard let item = commands.first(where: { $0.0 == name }) else { return }
        model.showingCommands = false
        item.1()
    }
}
