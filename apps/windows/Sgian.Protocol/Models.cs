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

    /// <summary>The agent's observed permission mode (auto, bypass, …); null when unknown.</summary>
    [JsonPropertyName("mode")]
    public string? Mode { get; init; }

    /// <summary>True when tools run without a person approving them (auto / bypass).</summary>
    [JsonPropertyName("unattended")]
    public bool Unattended { get; init; }
}

public sealed record AgentPaneSpec
{
    [JsonPropertyName("backend")]
    public string Backend { get; init; } = "claude";

    [JsonPropertyName("model")]
    public string? Model { get; init; }
}

/// <summary>A named group of panes serving one goal (ENHANCEMENTS "projects"); a pane is in at most one.</summary>
public sealed record Project
{
    [JsonPropertyName("name")]
    public string Name { get; init; } = "";

    [JsonPropertyName("goal")]
    public string? Goal { get; init; }

    [JsonPropertyName("repo")]
    public string? Repo { get; init; }

    /// <summary>Member pane ids in project order.</summary>
    [JsonPropertyName("panes")]
    public IReadOnlyList<string> Panes { get; init; } = [];
}

/// <summary>
/// Output-guard counters (docs/design/keyboard-lease-and-ledger.md §7): the tricks a
/// pane's output used to hide something from a person. Counts only grow for a pane's life.
/// </summary>
public sealed record OutputTricks
{
    [JsonPropertyName("conceal")]
    public int Conceal { get; init; }

    [JsonPropertyName("clipboard")]
    public int Clipboard { get; init; }

    [JsonPropertyName("hyperlink_mismatch")]
    public int HyperlinkMismatch { get; init; }

    [JsonPropertyName("string_controls")]
    public int StringControls { get; init; }

    [JsonPropertyName("c1_controls")]
    public int C1Controls { get; init; }

    [JsonIgnore]
    public int Total =>
        Math.Max(0, Conceal) + Math.Max(0, Clipboard) + Math.Max(0, HyperlinkMismatch)
        + Math.Max(0, StringControls) + Math.Max(0, C1Controls);

    /// <summary>"2 concealed text, 1 clipboard writes": the non-zero counters in display order.</summary>
    [JsonIgnore]
    public string Summary => string.Join(", ", new[]
        {
            (Conceal, "concealed text"),
            (Clipboard, "clipboard writes"),
            (HyperlinkMismatch, "mismatched links"),
            (StringControls, "opaque control strings"),
            (C1Controls, "C1 controls"),
        }
        .Where(entry => entry.Item1 > 0)
        .Select(entry => $"{entry.Item1} {entry.Item2}"));
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

    /// <summary>Held keyboard leases only; absent from pre-lease daemons.</summary>
    [JsonPropertyName("leases")]
    public IReadOnlyDictionary<string, LeaseInfo> Leases { get; init; }
        = new Dictionary<string, LeaseInfo>();

    /// <summary>Projects by name; absent from pre-project daemons.</summary>
    [JsonPropertyName("projects")]
    public IReadOnlyDictionary<string, Project> Projects { get; init; }
        = new Dictionary<string, Project>();

    /// <summary>Panes whose output hid something; absent from pre-guard daemons.</summary>
    [JsonPropertyName("output_warnings")]
    public IReadOnlyDictionary<string, OutputTricks> OutputWarnings { get; init; }
        = new Dictionary<string, OutputTricks>();
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
