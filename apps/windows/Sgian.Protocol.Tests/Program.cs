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
    Equal(false, LeaseState.IsRefusal("terminal session ended: pane-2"));
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
