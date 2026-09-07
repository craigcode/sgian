import AppKit
import Combine
import Sparkle

@MainActor
final class NativeUpdater: ObservableObject {
    @Published private(set) var canCheck = true
    private var controller: SPUStandardUpdaterController?

    init() {
        guard let key = Bundle.main.object(forInfoDictionaryKey: "SUPublicEDKey") as? String,
              Data(base64Encoded: key)?.count == 32 else { return }
        let controller = SPUStandardUpdaterController(startingUpdater: true, updaterDelegate: nil, userDriverDelegate: nil)
        self.controller = controller
        controller.updater.publisher(for: \.canCheckForUpdates).assign(to: &$canCheck)
    }

    func check() {
        guard let controller else {
            let alert = NSAlert()
            alert.messageText = "Updates are unavailable in this development build"
            alert.informativeText = "Install a signed Sgian release to receive automatic updates."
            alert.runModal()
            return
        }
        controller.checkForUpdates(nil)
    }
}
