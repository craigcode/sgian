using System.Text.Json;
using System.Text.Json.Serialization;

namespace Sgian.Protocol;

public sealed record NativeIpcEndpoint
{
    [JsonPropertyName("transport")]
    public string Transport { get; init; } = "";

    [JsonPropertyName("endpoint")]
    public string Endpoint { get; init; } = "";

    [JsonPropertyName("token_path")]
    public string TokenPath { get; init; } = "";

    [JsonPropertyName("workspace")]
    public string Workspace { get; init; } = "";

    [JsonPropertyName("workspace_key")]
    public string WorkspaceKey { get; init; } = "";

    [JsonPropertyName("protocol_version")]
    public int ProtocolVersion { get; init; }

    [JsonPropertyName("capabilities")]
    public IReadOnlyList<string> Capabilities { get; init; } = [];
}

public sealed record Pane
{
    [JsonPropertyName("id")]
    public string Id { get; init; } = "";

    [JsonPropertyName("title")]
    public string Title { get; init; } = "";

    [JsonPropertyName("kind")]
    public string Kind { get; init; } = "shell";

    [JsonPropertyName("created_at_ms")]
    public ulong CreatedAtMilliseconds { get; init; }
}

public sealed record PaneSize
{
    [JsonPropertyName("cols")]
    public ushort Columns { get; init; } = 80;

    [JsonPropertyName("rows")]
    public ushort Rows { get; init; } = 24;
}

public sealed record AgentPaneInfo
{
    [JsonPropertyName("agent")]
    public string? Agent { get; init; }

    [JsonPropertyName("attention")]
    public string? Attention { get; init; }
}

public sealed record AgentPaneSpec
{
    [JsonPropertyName("backend")]
    public string Backend { get; init; } = "claude";

    [JsonPropertyName("model")]
    public string? Model { get; init; }
}

public sealed record WorkspaceSnapshot
{
    [JsonPropertyName("panes")]
    public IReadOnlyList<Pane> Panes { get; init; } = [];

    [JsonPropertyName("active_pane_id")]
    public string? ActivePaneId { get; init; }

    [JsonPropertyName("cwd")]
    public string CurrentDirectory { get; init; } = "";

    [JsonPropertyName("layout")]
    public JsonElement? Layout { get; init; }

    [JsonPropertyName("scrollback")]
    public IReadOnlyDictionary<string, string> Scrollback { get; init; }
        = new Dictionary<string, string>();

    [JsonPropertyName("sizes")]
    public IReadOnlyDictionary<string, PaneSize> Sizes { get; init; }
        = new Dictionary<string, PaneSize>();

    [JsonPropertyName("pane_states")]
    public IReadOnlyDictionary<string, string> PaneStates { get; init; }
        = new Dictionary<string, string>();

    [JsonPropertyName("agent_states")]
    public IReadOnlyDictionary<string, AgentPaneInfo> AgentStates { get; init; }
        = new Dictionary<string, AgentPaneInfo>();

    [JsonPropertyName("agent_events")]
    public IReadOnlyDictionary<string, IReadOnlyList<JsonElement>> AgentEvents { get; init; }
        = new Dictionary<string, IReadOnlyList<JsonElement>>();

    [JsonPropertyName("agent_specs")]
    public IReadOnlyDictionary<string, AgentPaneSpec> AgentSpecs { get; init; }
        = new Dictionary<string, AgentPaneSpec>();
}

public sealed record CommandOk
{
    [JsonPropertyName("ok")]
    public bool Ok { get; init; }
}

internal sealed record IpcResponse
{
    [JsonPropertyName("ok")]
    public bool Ok { get; init; }

    [JsonPropertyName("result")]
    public JsonElement Result { get; init; }

    [JsonPropertyName("error")]
    public string? Error { get; init; }
}

public sealed record DaemonEvent(string Kind, JsonElement Payload)
{
    public string? String(string name) =>
        Payload.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.String
            ? value.GetString()
            : null;

    public int? Integer(string name) =>
        Payload.TryGetProperty(name, out var value) && value.TryGetInt32(out var result)
            ? result
            : null;

    public JsonElement? Value(string name) =>
        Payload.TryGetProperty(name, out var value) ? value : null;

    internal static DaemonEvent Parse(string json)
    {
        using var document = JsonDocument.Parse(json);
        var payload = document.RootElement.Clone();
        if (!payload.TryGetProperty("event", out var eventName) ||
            eventName.ValueKind != JsonValueKind.String)
        {
            throw new DaemonProtocolException("Daemon event is missing its event tag.");
        }

        return new DaemonEvent(eventName.GetString()!, payload);
    }
}
