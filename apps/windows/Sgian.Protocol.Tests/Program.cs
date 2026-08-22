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
    ("Permission lifecycle", PermissionLifecycle),
    ("Workspace defaults", WorkspaceDefaults),
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

static void PipeEndpointParsing()
{
    Equal("sgian2-S-1-5-21-abcd", EndpointDiscovery.PipeName(@"\\.\pipe\sgian2-S-1-5-21-abcd"));
    Throws<DaemonProtocolException>(() => EndpointDiscovery.PipeName("daemon.sock"));
    Throws<DaemonProtocolException>(() => EndpointDiscovery.PipeName(@"\\.\pipe\"));
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
