using System.Text.Json;
using Sgian.Protocol;

if (args is ["--integration", var backend, var workspace])
{
    Directory.CreateDirectory(workspace);
    await using var client = await DaemonClient.ConnectAsync(
        workspace,
        backend,
        onProgress: message => Console.WriteLine($"IPC {message}"));
    var snapshot = await client.RequestAsync<WorkspaceSnapshot>(new Dictionary<string, object?>
    {
        ["command"] = "bootstrap_workspace",
    }, onProgress: message => Console.WriteLine($"IPC {message}"));
    Equal(Path.GetFullPath(workspace), Path.GetFullPath(snapshot.CurrentDirectory));
    Console.WriteLine("PASS Live Windows named-pipe discovery, authentication, and bootstrap");
    return 0;
}

var tests = new (string Name, Action Body)[]
{
    ("Named-pipe endpoint parsing", PipeEndpointParsing),
    ("Agent stream reduction and replay dedupe", AgentStreamReduction),
    ("Prompt persistence and correlated optimistic echoes", UserPromptReplay),
    ("Permission lifecycle", PermissionLifecycle),
    ("Workspace defaults", WorkspaceDefaults),
    ("Bounded IPC messages", BoundedMessages),
    ("Terminal bridge rejects foreign documents", TerminalBridgeOrigins),
    ("Native layout restoration and pane reconciliation", NativeLayouts),
    ("Keyboard lease model", KeyboardLease),
    ("Agent permission mode", AgentPermissionMode),
    ("Project board model", ProjectBoardModel),
    ("Agent usage from the status line", AgentUsageModel),
    ("Workspace startup resolution", WorkspaceStartupResolution),
    ("Output warning sample", OutputWarningSample),
};

var failures = new List<string>();
foreach (var test in tests)
{
    try
    {
        test.Body();
        Console.WriteLine($"PASS {test.Name}");
    }
    catch (Exception error)
    {
        failures.Add($"FAIL {test.Name}: {error.Message}");
    }
}

if (failures.Count > 0)
{
    failures.ForEach(Console.Error.WriteLine);
    return 1;
}

Console.WriteLine($"Sgian.Protocol: {tests.Length} checks passed");
return 0;

static void UserPromptReplay()
{
    var item = Json("""{"kind":"user_message","text":"hello","message_id":"send-1","seq":1}""");
    var chat = new AgentChatState();
    chat.AppendUserMessage("hello", "send-1");
    chat.Apply(item);
    chat.Replay(new[] { item });
    chat.RemoveLastUserMessage("hello", "send-1");
    Equal(1, chat.Messages.Count);
    Equal(false, chat.Messages[0].IsPending);
    var restored = new AgentChatState();
    restored.Replay(new[] { item });
    Equal("hello", restored.Messages[0].Text);
    restored.Apply(Json("""{"kind":"user_message","text":"hello","message_id":"send-2","seq":2}"""));
    Equal(2, restored.Messages.Count);
    restored.AppendUserMessage("failed", "send-3");
    restored.RemoveLastUserMessage("failed", "send-3");
    Equal(2, restored.Messages.Count);
}

static void NativeLayouts()
{
    using var document = JsonDocument.Parse("""
        {"type":"split","id":"split-1","direction":"column","ratio":0.3,
         "first":{"type":"leaf","id":"one"},"second":{"type":"leaf","id":"two"}}
        """);
    var tree = PaneLayout.Parse(document.RootElement) ?? throw new Exception("Layout did not decode");
    Equal("one,two", string.Join(",", tree.PaneIds));
    Equal(0.7, tree.Resize("split-1", 0.7).Ratio);
    var repaired = PaneLayout.Reconcile(tree, ["two", "three"])!;
    Equal("two,three", string.Join(",", repaired.PaneIds));
    Equal("two", repaired.Remove("three")!.Id);
    Equal(true, PaneLayout.Reconcile(tree, []) is null);
    var inserted = tree.Insert("three", "one", "row");
    Equal("one,three,two", string.Join(",", inserted.PaneIds));
    Equal("row", inserted.First!.Direction);
    var duplicate = JsonSerializer.SerializeToElement(PaneLayout.Join(PaneLayout.Leaf("one"), PaneLayout.Leaf("one"), "row"));
    Equal(true, PaneLayout.Parse(duplicate) is null);
    Equal(0.5, PaneLayout.Clamp(double.NaN));
    Equal(0.82, tree.Resize("split-1", 100).Ratio);
}

static void PipeEndpointParsing()
{
    Equal("sgian2-S-1-5-21-abcd", EndpointDiscovery.PipeName(@"\\.\pipe\sgian2-S-1-5-21-abcd"));
    Throws<DaemonProtocolException>(() => EndpointDiscovery.PipeName("daemon.sock"));
    Throws<DaemonProtocolException>(() => EndpointDiscovery.PipeName(@"\\.\pipe\"));
}

static void BoundedMessages()
{
    using var lines = new BoundedLineReader(new StringReader("one\ntwo\r\n\n"), 4);
    Equal("one", lines.ReadLineAsync().GetAwaiter().GetResult());
    Equal("two", lines.ReadLineAsync().GetAwaiter().GetResult());
    Equal("", lines.ReadLineAsync().GetAwaiter().GetResult());
    Equal<string?>(null, lines.ReadLineAsync().GetAwaiter().GetResult());
    using var oversized = new BoundedLineReader(new StringReader(new string('x', 8192)), 32);
    Throws<DaemonProtocolException>(() => oversized.ReadLineAsync().GetAwaiter().GetResult());
    using var truncated = new BoundedLineReader(new StringReader("no-newline"));
    Throws<DaemonProtocolException>(() => truncated.ReadLineAsync().GetAwaiter().GetResult());
    using var cancelled = new CancellationTokenSource();
    cancelled.Cancel();
    using var pending = new BoundedLineReader(new StringReader("message\n"));
    Throws<OperationCanceledException>(() => pending.ReadLineAsync(cancelled.Token).GetAwaiter().GetResult());
}

static void TerminalBridgeOrigins()
{
    Equal(true, Sgian.Windows.Terminal.TerminalBridgePolicy.IsTrustedDocument("https://sgian.local/index.html"));
    foreach (var url in new string?[] { null, "", "https://evil.example/", "https://sgian.local.evil/index.html",
        "https://sgian.local@evil.example/index.html", "http://sgian.local/index.html", "file:///index.html",
        "https://sgian.local/other.html", "https://sgian.local:444/index.html", "https://sgian.local/index.html?x=1" })
        Equal(false, Sgian.Windows.Terminal.TerminalBridgePolicy.IsTrustedDocument(url));
}

static void AgentStreamReduction()
{
    var state = new AgentChatState();
    state.AppendUserMessage("hello");
    state.Replay(new[]
    {
        Json("""{"kind":"message_start","seq":1}"""),
        Json("""{"kind":"text_delta","seq":2,"text":"Hello"}"""),
        Json("""{"kind":"text_delta","seq":3,"text":" Windows"}"""),
        Json("""{"kind":"message_complete","seq":4}"""),
        Json("""{"kind":"turn_complete","seq":5,"subtype":"success","cost_usd":0.01}"""),
    });
    Equal(2, state.Messages.Count);
    Equal("Hello Windows", state.Messages[1].Text);
    Equal(false, state.Messages[1].IsOpen);
    Equal(false, state.Busy);
    Equal((ulong)5, state.LastSequence);

    state.Apply(Json("""{"kind":"text_delta","seq":3,"text":" duplicate"}"""));
    Equal("Hello Windows", state.Messages[1].Text);
}

static void PermissionLifecycle()
{
    var state = new AgentChatState();
    state.Apply(Json("""{"kind":"permission_request","seq":1,"request_id":"r1","tool_name":"Bash","input":{"command":"pwd"}}"""));
    Equal("r1", state.PendingPermission?.RequestId);
    Equal("Bash", state.PendingPermission?.ToolName);
    state.Apply(Json("""{"kind":"permission_resolved","seq":2,"request_id":"r1","reason":"user"}"""));
    Equal<PendingPermission?>(null, state.PendingPermission);
}

static void AgentPermissionMode()
{
    var flagged = JsonSerializer.Deserialize<AgentPaneInfo>("""{"agent":"claude","attention":"idle","mode":"auto","unattended":true}""")!;
    Equal("auto", flagged.Mode);
    Equal(true, flagged.Unattended);
    var legacy = JsonSerializer.Deserialize<AgentPaneInfo>("""{"agent":"claude","attention":"working"}""")!;
    Equal(null, legacy.Mode);
    Equal(false, legacy.Unattended);
    Equal(true, AgentPaneInfo.IsUnattendedMode("auto"));
    Equal(true, AgentPaneInfo.IsUnattendedMode("bypassPermissions"));
    Equal(true, AgentPaneInfo.IsUnattendedMode("dontAsk"));
    Equal(false, AgentPaneInfo.IsUnattendedMode("plan"));
    Equal(false, AgentPaneInfo.IsUnattendedMode(null));
}

static void WorkspaceStartupResolution()
{
    var alive = new HashSet<string>(StringComparer.OrdinalIgnoreCase) { @"C:\w\alive", @"C:\w\older" };
    Func<string, bool> exists = alive.Contains;
    Equal(@"C:\w\gone", WorkspaceStartup.Resolve(@"C:\w\gone", @"C:\w\alive", null, @"C:\x", @"C:\", @"C:\Users\me", exists));
    Equal(@"C:\w\alive", WorkspaceStartup.Resolve(null, @"C:\w\alive", new[] { @"C:\w\older" }, @"C:\x", @"C:\", @"C:\Users\me", exists));
    Equal(@"C:\w\older", WorkspaceStartup.Resolve(null, @"C:\w\gone", new[] { @"C:\w\gone", @"C:\w\older", @"C:\w\alive" }, @"C:\x", @"C:\", @"C:\Users\me", exists));
    Equal(@"C:\x", WorkspaceStartup.Resolve(null, @"C:\w\gone", new[] { @"C:\w\gone" }, @"C:\x", @"C:\", @"C:\Users\me", exists));
    Equal(@"C:\Users\me", WorkspaceStartup.Resolve(null, null, null, @"C:\", @"C:\", @"C:\Users\me", exists));
    Equal(@"C:\x", WorkspaceStartup.Resolve("", null, null, @"C:\x", @"C:\", @"C:\Users\me", exists));
}

static void OutputWarningSample()
{
    var warnings = new Dictionary<string, OutputTricks>();
    Equal("p1", ProjectBoard.ApplyWarning(Json("""{"pane_id":"p1","added":{"string_controls":1},"total":{"string_controls":1},"sample":"APC \"Ga=T,f=100;iVBOR\""}"""), warnings));
    Equal("APC \"Ga=T,f=100;iVBOR\"", warnings["p1"].Sample);
    Equal("p1", ProjectBoard.ApplyWarning(Json("""{"pane_id":"p1","total":{"string_controls":2}}"""), warnings));
    Equal(2, warnings["p1"].Total);
    Equal("APC \"Ga=T,f=100;iVBOR\"", warnings["p1"].Sample);
    Equal("p1", ProjectBoard.ApplyWarning(Json("""{"pane_id":"p1","total":{"conceal":1},"sample":null}"""), warnings));
    Equal("APC \"Ga=T,f=100;iVBOR\"", warnings["p1"].Sample);
    Equal("p2", ProjectBoard.ApplyWarning(Json("""{"pane_id":"p2","total":{"conceal":1}}"""), warnings));
    Equal(null, warnings["p2"].Sample);
}

static void KeyboardLease()
{
    var snapshot = JsonSerializer.Deserialize<WorkspaceSnapshot>("""
        {"panes":[],"cwd":"C:\\w","leases":{"pane-1":{"holder":"bob","since_ms":42}}}
        """) ?? throw new Exception("snapshot did not deserialize");
    Equal("bob", snapshot.Leases["pane-1"].Holder);
    Equal<ulong?>(42, snapshot.Leases["pane-1"].SinceMilliseconds);
    Equal(0, JsonSerializer.Deserialize<WorkspaceSnapshot>("""{"panes":[],"cwd":"C:\\w"}""")!.Leases.Count);
    var leases = new Dictionary<string, LeaseInfo>(StringComparer.Ordinal);
    Equal("pane-1", LeaseState.Apply(Json("""{"pane_id":"pane-1","transition":"taken","holder":"amy","since_ms":7}"""), leases));
    Equal("amy", leases["pane-1"].Holder);
    Equal("pane-1", LeaseState.Apply(Json("""{"pane_id":"pane-1","transition":"released","holder":null,"note":"done"}"""), leases));
    Equal(0, leases.Count);
    Equal(true, LeaseState.Apply(Json("""{"event":"lease_state"}"""), leases) is null);
    Equal(true, LeaseState.IsValidHolder("craig@pc"));
    Equal(false, LeaseState.IsValidHolder("two words"));
    Equal(false, LeaseState.IsValidHolder(new string('x', 65)));
    Equal(true, LeaseState.IsValidHolder(LeaseState.DefaultHolder()));
    Equal("Read-only: keyboard held by bob. Ctrl+Shift+T to take it.", LeaseState.NoticeText("pane keyboard is held by bob (pane-2)"));
    Equal(true, LeaseState.NoticeText("pane keyboard is unheld and lease_policy is required; take it first (pane-2)").Contains("take the keyboard"));
    Equal(true, LeaseState.NeedsForce("pane keyboard is held by bob; use --force --why REASON to revoke it"));
    Equal(true, LeaseState.IsRefusal("pane keyboard is held by bob (pane-2)"));
    Equal(true, LeaseState.IsRefusal("read-only credential: 'write' scope required for send_input"));
    Equal("Read-only: this credential cannot type (no write scope).", LeaseState.NoticeText("read-only credential: 'write' scope required for send_input"));
    Equal(false, LeaseState.IsRefusal("terminal session ended: pane-2"));
}

static void ProjectBoardModel()
{
    var snapshot = JsonSerializer.Deserialize<WorkspaceSnapshot>("""
        {"panes":[],"cwd":"C:\\w","projects":{"feat":{"name":"feat","goal":"ship","panes":["p1","p2"],"created_at_ms":1}},"output_warnings":{"p1":{"conceal":2,"c1_controls":1}}}
        """) ?? throw new Exception("snapshot did not deserialize");
    Equal("ship", snapshot.Projects["feat"].Goal);
    Equal(2, snapshot.Projects["feat"].Panes.Count);
    Equal(3, snapshot.OutputWarnings["p1"].Total);
    Equal("2 concealed text, 1 C1 controls", snapshot.OutputWarnings["p1"].Summary);
    Equal(0, JsonSerializer.Deserialize<WorkspaceSnapshot>("""{"panes":[],"cwd":"C:\\w"}""")!.Projects.Count);

    var projects = ProjectBoard.ParseProjects(Json("""{"projects":{"zeta":{"panes":["p3","gone","p3"]},"alpha":{"name":"alpha","goal":"first","panes":["p2"]},"empty":{},"bad":"no"}}"""))
        ?? throw new Exception("projects did not parse");
    Equal(3, projects.Count);
    Equal(2, projects["zeta"].Panes.Count);
    Equal("zeta", projects["zeta"].Name);
    Equal(true, ProjectBoard.ParseProjects(Json("""{"event":"projects_changed"}""")) is null);
    var groups = ProjectBoard.Group(["p1", "p2", "p3"], projects);
    Equal("alpha,empty,zeta,", string.Join(",", groups.Select(group => group.Name ?? "")));
    Equal("p2", groups[0].PaneIds[0]);
    Equal("first", groups[0].Goal);
    Equal("p3", groups[2].PaneIds[0]);
    Equal(1, groups[2].PaneIds.Count);
    Equal("p1", groups[3].PaneIds[0]);
    Equal("No project", groups[3].Title);
    Equal(1, ProjectBoard.Group(["p1"], new Dictionary<string, Project> { ["a"] = new Project { Name = "a", Panes = ["p1"] } }).Count);
    Equal(1, ProjectBoard.Group(["p1"], new Dictionary<string, Project>()).Count);
    Equal("alpha", ProjectBoard.ProjectFor("p2", projects));
    Equal(true, ProjectBoard.ProjectFor("p1", projects) is null);

    var rollup = ProjectBoard.Rollup(
    [
        new PaneFacts("p1", "live", "needs_input", false, "bob", false),
        new PaneFacts("p2", "live", "working", true, "alice", true),
        new PaneFacts("p3", "ended", null, false, "bob", false),
    ]);
    Equal("3 panes \u00B7 2 live \u00B7 1 needs input \u00B7 1 working \u00B7 \u26A0 1 unattended \u00B7 1 with output warnings \u00B7 \u2328 alice, bob", rollup.Text);
    Equal("1 pane", ProjectBoard.Rollup([new PaneFacts("p9", "live", null, false, null, false)]).Text);

    var warnings = new Dictionary<string, OutputTricks>(StringComparer.Ordinal);
    Equal("p1", ProjectBoard.ApplyWarning(Json("""{"pane_id":"p1","added":{"clipboard":1},"total":{"conceal":2,"clipboard":1}}"""), warnings));
    Equal(3, warnings["p1"].Total);
    Equal(true, ProjectBoard.ApplyWarning(Json("""{"pane_id":"p1"}"""), warnings) is null);
    Equal(3, warnings["p1"].Total);
    Equal("p1", ProjectBoard.ApplyWarning(Json("""{"pane_id":"p1","total":{}}"""), warnings));
    Equal(0, warnings.Count);
}

static void AgentUsageModel()
{
    var snapshot = JsonSerializer.Deserialize<WorkspaceSnapshot>("""
        {"panes":[],"cwd":"C:\\w","agent_usage":{"p1":{"model":"Opus","context_used_percentage":40,"five_hour":{"used_percentage":23,"resets_at":1000000},"seven_day":{"used_percentage":41},"updated_at_ms":5}}}
        """) ?? throw new Exception("snapshot did not deserialize");
    var usage = snapshot.AgentUsage["p1"];
    Equal("Opus", usage.Model);
    Equal(23, usage.FiveHour!.UsedPercentage);
    Equal("Opus \u00B7 40% context \u00B7 5h 23% \u21BB 1h10m \u00B7 7d 41%", usage.Summary(1000000 - 4200));
    Equal("Opus \u00B7 40% context \u00B7 5h 23% \u00B7 7d 41%", usage.Summary(3000000));
    Equal(false, usage.IsHot);
    Equal(" \u21BB 1h30m", AgentUsage.FormatReset(100 + 90 * 60, 100));
    Equal("", AgentUsage.FormatReset(null, 0));
    Equal(0, JsonSerializer.Deserialize<WorkspaceSnapshot>("""{"panes":[],"cwd":"C:\\w"}""")!.AgentUsage.Count);

    var table = new Dictionary<string, AgentUsage>(StringComparer.Ordinal);
    Equal("p1", AgentUsage.Apply(Json("""{"pane_id":"p1","usage":{"model":"Opus","context_used_percentage":85,"updated_at_ms":9}}"""), table));
    Equal(85, table["p1"].ContextUsedPercentage);
    Equal(true, table["p1"].IsHot);
    Equal(true, AgentUsage.Apply(Json("""{"pane_id":"p1"}"""), table) is null);
    Equal("p1", AgentUsage.Apply(Json("""{"pane_id":"p1","usage":{"updated_at_ms":1}}"""), table));
    Equal(0, table.Count);
}

static void WorkspaceDefaults()
{
    var snapshot = JsonSerializer.Deserialize<WorkspaceSnapshot>("""
        {"panes":[],"active_pane_id":null,"cwd":"C:\\work"}
        """) ?? throw new Exception("snapshot did not deserialize");
    Equal(0, snapshot.Scrollback.Count);
    Equal(0, snapshot.AgentEvents.Count);
    Equal(@"C:\work", snapshot.CurrentDirectory);
}

static JsonElement Json(string json)
{
    using var document = JsonDocument.Parse(json);
    return document.RootElement.Clone();
}

static void Equal<T>(T expected, T actual)
{
    if (!EqualityComparer<T>.Default.Equals(expected, actual))
    {
        throw new Exception($"expected {expected}, got {actual}");
    }
}

static void Throws<T>(Action body) where T : Exception
{
    try
    {
        body();
    }
    catch (T)
    {
        return;
    }
    throw new Exception($"expected {typeof(T).Name}");
}
