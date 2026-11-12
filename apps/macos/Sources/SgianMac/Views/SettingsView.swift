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
    @State private var codexBinary = ""
    @State private var geminiBinary = ""
    @State private var permissionMode = "manual"
    @State private var restorePolicy = "auto_respawn"
    @State private var profiles: [JSONValue] = []
    @State private var profileName = ""
    @State private var profileKind = "shell"
    @State private var profileBackend = "claude"
    @State private var profileModel = ""
    @State private var credential = ""
    @State private var message = ""
    @State private var loaded = false
    @State private var saving = false
    @State private var loadedWorkspace = ""

    private var activeWorkspaceKey: String {
        "\(model.activeMachine?.name ?? "local"):\(model.workspaceURL.path)"
    }

    var body: some View {
        VStack(spacing: 0) {
            Form {
                Section("Appearance") {
                    HStack {
                        Text("Terminal font size")
                        Slider(value: $model.terminalFontSize, in: 9...30, step: 1)
                            .accessibilityLabel("Terminal font size")
                            .accessibilityValue("\(Int(model.terminalFontSize)) points")
                        Text("\(Int(model.terminalFontSize)) pt")
                            .monospacedDigit()
                            .frame(width: 45)
                            .accessibilityHidden(true)
                    }
                }
                Section("New agent defaults") {
                    Picker("Provider", selection: $defaultAgentBackend) {
                        Text("Claude Code").tag("claude")
                        Text("Factory Droid").tag("droid")
                        Text("Codex").tag("codex")
                        Text("Gemini CLI").tag("gemini")
                    }
                    TextField("Model (provider default when empty)", text: $defaultAgentModel)
                    Text("Agents use the provider CLI’s existing installation and login on this Mac.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                Section(model.activeMachine.map { "Remote Workspace: \($0.name)" } ?? "Workspace: \(model.workspaceURL.lastPathComponent)") {
                    TextField("Shell executable", text: $shell)
                    TextField("Shell arguments (one per line)", text: $shellArguments, axis: .vertical)
                    TextField("Claude executable (optional)", text: $claudeBinary)
                    TextField("Droid executable (optional)", text: $droidBinary)
                    TextField("Codex executable (optional)", text: $codexBinary)
                    TextField("Gemini executable (optional)", text: $geminiBinary)
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
                        let profileTitle = profile["name"]?.stringValue ?? "Profile"
                        HStack {
                            Text(profileTitle)
                            Spacer()
                            Text(profile["kind"]?.stringValue ?? "shell").foregroundStyle(.secondary)
                            Button("Remove") { profiles.remove(at: index) }
                                .accessibilityLabel("Remove profile \(profileTitle)")
                        }
                    }
                    TextField("Profile name", text: $profileName)
                    Picker("Type", selection: $profileKind) {
                        Text("Terminal").tag("shell"); Text("Agent").tag("agent")
                    }
                    if profileKind == "agent" {
                        Picker("Provider", selection: $profileBackend) {
                            Text("Claude Code").tag("claude")
                            Text("Factory Droid").tag("droid")
                            Text("Codex").tag("codex")
                            Text("Gemini CLI").tag("gemini")
                        }
                        TextField("Model (optional)", text: $profileModel)
                    }
                    Button("Add Profile", action: addProfile).disabled(profileName.trimmingCharacters(in: .whitespaces).isEmpty)
                }
                Section("Identity") {
                    Text("Acting as \(model.holder)")
                    if model.credentialFromEnvironment {
                        Text("The credential comes from SGIAN_CLIENT_TOKEN in this app’s environment and overrides the Keychain.")
                            .font(.caption).foregroundStyle(.secondary)
                    } else {
                        SecureField("Client credential (sgc_…, from `sgian ctl identity issue`)", text: $credential)
                        HStack {
                            Button("Use Credential") {
                                model.setClientCredential(credential)
                                credential = ""
                                message = "Credential stored in the login Keychain; reconnecting."
                            }
                            .disabled(credential.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                            if model.hasStoredCredential {
                                Button("Forget Keychain Credential") {
                                    model.setClientCredential(nil)
                                    message = "Keychain credential removed; reconnecting."
                                }
                            }
                        }
                        Text(model.hasStoredCredential
                             ? "A client credential is configured for this target; the daemon attributes your input and leases to it."
                             : (model.activeMachine != nil
                                ? "Remote targets require a scoped client credential (`sgian ctl identity issue`)."
                                : "Without a credential this Mac connects with the workspace token as \(WorkspaceModel.defaultHolder())."))
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }
                Section("Workspace") {
                    if let machine = model.activeMachine {
                        Text("\(machine.name) — \(machine.displaySubtitle)").textSelection(.enabled)
                    } else {
                        Text(model.workspaceURL.path).textSelection(.enabled)
                    }
                    Button("Choose Local Workspace…", action: model.chooseWorkspace)
                }
            }.formStyle(.grouped)
            HStack {
                Text(message).font(.caption).foregroundStyle(.secondary)
                Spacer()
                Button("Reload") { Task { await load() } }.disabled(saving)
                Button(saving ? "Saving…" : "Save Workspace Settings") { Task { await save() } }
                    .disabled(!loaded || saving || loadedWorkspace != activeWorkspaceKey)
            }.padding(14)
        }
        .frame(width: 600, height: 680)
        .task(id: activeWorkspaceKey) { await load() }
    }

    private func load() async {
        loaded = false
        do {
            config = try await model.readConfiguration()
            shell = config["shell"]?.stringValue ?? ""
            shellArguments = (config["shell_args"]?.arrayValue ?? []).compactMap(\.stringValue).joined(separator: "\n")
            claudeBinary = config["agent_claude_bin"]?.stringValue ?? ""
            droidBinary = config["agent_droid_bin"]?.stringValue ?? ""
            codexBinary = config["agent_codex_bin"]?.stringValue ?? ""
            geminiBinary = config["agent_gemini_bin"]?.stringValue ?? ""
            permissionMode = config["agent_permission_mode"]?.stringValue ?? "manual"
            restorePolicy = config["restore_policy"]?.stringValue ?? "auto_respawn"
            profiles = config["profiles"]?.arrayValue ?? []
            loadedWorkspace = activeWorkspaceKey
            loaded = true; message = ""
        } catch { message = error.localizedDescription }
    }

    private func save() async {
        guard loadedWorkspace == activeWorkspaceKey else { return }
        saving = true
        defer { saving = false }
        // Preserve all fields not exposed by this form, including environment restrictions.
        var next = config
        next["shell"] = shell.isEmpty ? .null : .string(shell)
        let parsedShellArgs = shellArguments
            .split(separator: "\n", omittingEmptySubsequences: true)
            .map { $0.hasSuffix("\r") ? String($0.dropLast()) : String($0) }
            .filter { !$0.isEmpty }
        next["shell_args"] = parsedShellArgs.isEmpty ? .null : .array(parsedShellArgs.map { .string($0) })
        next["agent_claude_bin"] = claudeBinary.isEmpty ? .null : .string(claudeBinary)
        next["agent_droid_bin"] = droidBinary.isEmpty ? .null : .string(droidBinary)
        next["agent_codex_bin"] = codexBinary.isEmpty ? .null : .string(codexBinary)
        next["agent_gemini_bin"] = geminiBinary.isEmpty ? .null : .string(geminiBinary)
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
            // The daemon's profile schema names these `agent_backend` and
            // `agent_model` and rejects unknown fields (issue #31).
            profile["agent_backend"] = .string(profileBackend)
            if !profileModel.isEmpty { profile["agent_model"] = .string(profileModel) }
        }
        profiles.append(.object(profile)); profileName = ""; message = "Save to apply the new profile."
    }
}
