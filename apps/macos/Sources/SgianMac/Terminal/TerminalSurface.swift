import AppKit
import SwiftTerm
import SwiftUI

enum TerminalLinkPolicy {
    static func externalURL(_ link: String) -> URL? {
        guard let url = URL(string: link),
              let scheme = url.scheme?.lowercased(),
              scheme == "https" || scheme == "http",
              let host = url.host, !host.isEmpty,
              url.user == nil, url.password == nil
        else { return nil }
        return url
    }
}

@MainActor
final class TerminalSurface: NSObject, ObservableObject, @preconcurrency TerminalViewDelegate, Identifiable {
    let id: String
    let view: TerminalView
    var onInput: ((Data) -> Void)?
    var onResize: ((Int, Int) -> Void)?
    var onFocus: (() -> Void)?
    var onTitleChange: ((String) -> Void)?

    private var resizeTask: Task<Void, Never>?
    private(set) var latestColumns = 80
    private(set) var latestRows = 24
    private var loadedInitialScrollback = false

    init(id: String, fontSize: CGFloat = 13, columns: Int = 120, rows: Int = 40) {
        let initialColumns = max(2, min(columns, Int(UInt16.max)))
        let initialRows = max(1, min(rows, Int(UInt16.max)))
        self.id = id
        latestColumns = initialColumns
        latestRows = initialRows
        view = TerminalView(
            frame: NSRect(x: 0, y: 0, width: 900, height: 600),
            font: NSFont.monospacedSystemFont(ofSize: fontSize, weight: .regular)
        )
        super.init()
        // Scrollback is a raw ANSI replay. Match the PTY's recorded dimensions
        // before feeding it so zsh's margin-sensitive prompt marker and wrapped
        // lines reproduce exactly instead of becoming visible artifacts.
        view.resize(cols: initialColumns, rows: initialRows)
        view.terminalDelegate = self
        view.wantsLayer = true
        view.nativeForegroundColor = NSColor(calibratedWhite: 0.91, alpha: 1)
        view.nativeBackgroundColor = NSColor(calibratedRed: 0.055, green: 0.06, blue: 0.052, alpha: 1)
        view.layer?.backgroundColor = view.nativeBackgroundColor.cgColor
        view.caretColor = .systemGreen
        view.changeScrollback(20_000)
        view.setAccessibilityLabel("Terminal \(id)")
    }

    func feed(_ text: String) {
        view.feed(text: text)
    }

    func loadInitialScrollback(_ text: String) {
        guard !loadedInitialScrollback else { return }
        loadedInitialScrollback = true
        if !text.isEmpty { feed(text) }
    }

    func focus() {
        guard let window = view.window else { return }
        window.makeFirstResponder(view)
    }

    func clearScrollback() { view.clearScrollback() }

    func setFontSize(_ size: CGFloat) {
        view.font = NSFont.monospacedSystemFont(ofSize: size, weight: .regular)
    }

    func sizeChanged(source: TerminalView, newCols: Int, newRows: Int) {
        let columns = max(2, min(newCols, Int(UInt16.max)))
        let rows = max(1, min(newRows, Int(UInt16.max)))
        guard columns != latestColumns || rows != latestRows else { return }
        latestColumns = columns
        latestRows = rows
        resizeTask?.cancel()
        resizeTask = Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(70))
            guard !Task.isCancelled else { return }
            self?.onResize?(columns, rows)
        }
    }

    func send(source: TerminalView, data: ArraySlice<UInt8>) {
        onInput?(Data(data))
    }

    func setTerminalTitle(source: TerminalView, title: String) {
        onTitleChange?(title)
    }

    func requestOpenLink(source: TerminalView, link: String, params: [String: String]) {
        // OSC 8 links come from terminal output. Do not let a disguised link
        // launch a local application, file, or arbitrary custom URL handler.
        guard let url = TerminalLinkPolicy.externalURL(link) else { return }
        NSWorkspace.shared.open(url)
    }

    func hostCurrentDirectoryUpdate(source: TerminalView, directory: String?) {}
    func scrolled(source: TerminalView, position: Double) {}
    func rangeChanged(source: TerminalView, startY: Int, endY: Int) {}
}

struct TerminalSurfaceView: NSViewRepresentable {
    let surface: TerminalSurface

    func makeNSView(context: Context) -> TerminalView {
        return surface.view
    }

    func updateNSView(_ nsView: TerminalView, context: Context) {}
}
