import SwiftUI

struct SettingsView: View {
    @ObservedObject var model: WorkspaceModel
    @AppStorage("defaultAgentBackend") private var defaultAgentBackend = AgentBackend.claude.rawValue

    var body: some View {
        Form {
            Section("Terminal") {
                HStack {
                    Text("Font size")
                    Slider(value: $model.terminalFontSize, in: 10...24, step: 1)
                    Text("\(Int(model.terminalFontSize)) pt")
                        .monospacedDigit()
                        .frame(width: 42, alignment: .trailing)
                }
            }
            Section("Agents") {
                Picker("Default backend", selection: $defaultAgentBackend) {
                    ForEach(AgentBackend.allCases) { backend in
                        Text(backend.displayName).tag(backend.rawValue)
                    }
                }
            }
            Section("Workspace") {
                LabeledContent("Current", value: model.workspaceURL.path)
                Button("Choose Workspace…", action: model.chooseWorkspace)
            }
        }
        .formStyle(.grouped)
        .padding(8)
        .frame(width: 520, height: 300)
    }
}
