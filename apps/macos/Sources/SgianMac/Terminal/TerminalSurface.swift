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
    private var accessibilityTask: Task<Void, Never>?
    private(set) var latestColumns = 80
    private(set) var latestRows = 24
    private var loadedInitialScrollback = false
    private(set) var title: String

    init(id: String, fontSize: CGFloat = 13, columns: Int = 120, rows: Int = 40) {
        let initialColumns = max(2, min(columns, Int(UInt16.max)))
        let initialRows = max(1, min(rows, Int(UInt16.max)))
        self.id = id
        title = id
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
        // SwiftTerm draws the screen itself, so without this the terminal is
        // an unlabeled scroll bar to VoiceOver. Present it as a text area
        // whose value is the visible screen; the label follows the pane title.
        view.setAccessibilityElement(true)
        view.setAccessibilityRole(.textArea)
        view.setAccessibilityLabel("Terminal \(id)")
    }

    /// Name the terminal after its pane for accessibility clients.
    func setTitle(_ title: String) {
        guard self.title != title else { return }
        self.title = title
        view.setAccessibilityLabel("Terminal \(title)")
    }

    /// The visible screen as text, one line per row, trailing blanks trimmed.
    static func visibleText(of terminal: Terminal) -> String {
        (0..<terminal.rows)
            .compactMap { terminal.getLine(row: $0)?.translateToString(trimRight: true) }
            .joined(separator: "\n")
    }

    /// Republish the visible screen as the accessibility value, coalescing
    /// bursts of output into one update.
    private func scheduleAccessibilityRefresh() {
        guard accessibilityTask == nil else { return }
        accessibilityTask = Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(150))
            guard let self else { return }
            self.accessibilityTask = nil
            self.view.setAccessibilityValue(TerminalSurface.visibleText(of: self.view.getTerminal()))
        }
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
    func rangeChanged(source: TerminalView, startY: Int, endY: Int) {
        scheduleAccessibilityRefresh()
    }
}

struct TerminalSurfaceView: NSViewRepresentable {
    let surface: TerminalSurface
    var focusOnAppear = false

    func makeNSView(context: Context) -> TerminalView {
        if focusOnAppear { DispatchQueue.main.async { surface.focus() } }
        return surface.view
    }

    func updateNSView(_ nsView: TerminalView, context: Context) {}
}
