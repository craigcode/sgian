import SwiftUI

struct SettingsView: View {
    @ObservedObject var model: WorkspaceModel
    @AppStorage("defaultAgentBackend") private var defaultAgentBackend = "claude"
    @AppStorage("defaultAgentModel") private var defaultAgentModel = ""
    @State private var config: [String: JSONValue] = [:]
    @State private var shell = ""
    @State private var shellArguments = ""
    @State private var claudeBinary = ""
    @State private var droidBinary = ""
    @State private var permissionMode = "manual"
    @State private var restorePolicy = "auto_respawn"
    @State private var profiles: [JSONValue] = []
    @State private var profileName = ""
    @State private var profileKind = "shell"
    @State private var profileBackend = "claude"
    @State private var profileModel = ""
    @State private var message = ""
    @State private var loaded = false
    @State private var saving = false
    @State private var loadedWorkspace = ""

    var body: some View {
        VStack(spacing: 0) {
            Form {
                Section("Appearance") {
                    HStack {
                        Text("Terminal font size")
                        Slider(value: $model.terminalFontSize, in: 9...30, step: 1)
                        Text("\(Int(model.terminalFontSize)) pt").monospacedDigit().frame(width: 45)
                    }
                }
                Section("New agent defaults") {
                    Picker("Provider", selection: $defaultAgentBackend) {
                        Text("Claude Code").tag("claude"); Text("Factory Droid").tag("droid")
                    }
                    TextField("Model (provider default when empty)", text: $defaultAgentModel)
                    Text("Agents use the provider CLI’s existing installation and login on this Mac.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                Section("Workspace: \(model.workspaceURL.lastPathComponent)") {
                    TextField("Shell executable", text: $shell)
                    TextField("Shell arguments (one per line)", text: $shellArguments, axis: .vertical)
                    TextField("Claude executable (optional)", text: $claudeBinary)
                    TextField("Droid executable (optional)", text: $droidBinary)
                    Picker("Agent permissions", selection: $permissionMode) {
                        Text("Ask for approval").tag("manual")
                        Text("Automatic").tag("auto")
                        Text("Do not ask").tag("dontAsk")
                        Text("Bypass permissions").tag("bypassPermissions")
                    }
                    if permissionMode != "manual" {
                        Label("Agents may run tools without asking you. Applies to newly started agents.", systemImage: "exclamationmark.triangle")
                            .font(.caption).foregroundStyle(.orange)
                    }
                    Picker("Ended sessions", selection: $restorePolicy) {
                        Text("Restart automatically").tag("auto_respawn")
                        Text("Restart when requested").tag("restore_on_demand")
                    }
                    Text("Shell and provider changes apply to newly started sessions.").font(.caption).foregroundStyle(.secondary)
                }
                Section("Profiles") {
                    ForEach(Array(profiles.enumerated()), id: \.offset) { index, profile in
                        HStack {
                            Text(profile["name"]?.stringValue ?? "Profile")
                            Spacer()
                            Text(profile["kind"]?.stringValue ?? "shell").foregroundStyle(.secondary)
                            Button("Remove") { profiles.remove(at: index) }
                        }
                    }
                    TextField("Profile name", text: $profileName)
                    Picker("Type", selection: $profileKind) {
                        Text("Terminal").tag("shell"); Text("Agent").tag("agent")
                    }
                    if profileKind == "agent" {
                        Picker("Provider", selection: $profileBackend) {
                            Text("Claude Code").tag("claude"); Text("Factory Droid").tag("droid")
                        }
                        TextField("Model (optional)", text: $profileModel)
                    }
                    Button("Add Profile", action: addProfile).disabled(profileName.trimmingCharacters(in: .whitespaces).isEmpty)
                }
                Section("Workspace") {
                    Text(model.workspaceURL.path).textSelection(.enabled)
                    Button("Choose Workspace…", action: model.chooseWorkspace)
                }
            }.formStyle(.grouped)
            HStack {
                Text(message).font(.caption).foregroundStyle(.secondary)
                Spacer()
                Button("Reload") { Task { await load() } }.disabled(saving)
                Button(saving ? "Saving…" : "Save Workspace Settings") { Task { await save() } }
                    .disabled(!loaded || saving || loadedWorkspace != model.workspaceURL.path)
            }.padding(14)
        }
        .frame(width: 600, height: 680)
        .task(id: model.workspaceURL) { await load() }
    }

    private func load() async {
        loaded = false
        do {
            config = try await model.readConfiguration()
            shell = config["shell"]?.stringValue ?? ""
            shellArguments = (config["shell_args"]?.arrayValue ?? []).compactMap(\.stringValue).joined(separator: "\n")
            claudeBinary = config["agent_claude_bin"]?.stringValue ?? ""
            droidBinary = config["agent_droid_bin"]?.stringValue ?? ""
            permissionMode = config["agent_permission_mode"]?.stringValue ?? "manual"
            restorePolicy = config["restore_policy"]?.stringValue ?? "auto_respawn"
            profiles = config["profiles"]?.arrayValue ?? []
            loadedWorkspace = model.workspaceURL.path
            loaded = true; message = ""
        } catch { message = error.localizedDescription }
    }

    private func save() async {
        guard loadedWorkspace == model.workspaceURL.path else { return }
        saving = true
        defer { saving = false }
        // Preserve all fields not exposed by this form, including environment restrictions.
        var next = config
        next["shell"] = shell.isEmpty ? .null : .string(shell)
        next["shell_args"] = .array(shellArguments.split(separator: "\n").map { .string(String($0)) })
        next["agent_claude_bin"] = claudeBinary.isEmpty ? .null : .string(claudeBinary)
        next["agent_droid_bin"] = droidBinary.isEmpty ? .null : .string(droidBinary)
        next["agent_permission_mode"] = .string(permissionMode)
        next["restore_policy"] = .string(restorePolicy)
        next["profiles"] = .array(profiles)
        do { try await model.writeConfiguration(next); config = next; message = "Workspace settings saved." }
        catch { message = error.localizedDescription }
    }

    private func addProfile() {
        let name = profileName.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !profiles.contains(where: { $0["name"]?.stringValue == name }) else { message = "A profile with that name already exists."; return }
        var profile: [String: JSONValue] = ["name": .string(name), "kind": .string(profileKind)]
        if profileKind == "agent" {
            profile["backend"] = .string(profileBackend)
            if !profileModel.isEmpty { profile["model"] = .string(profileModel) }
        }
        profiles.append(.object(profile)); profileName = ""; message = "Save to apply the new profile."
    }
}
