using System.Text.Json;
using System.Text.Json.Serialization;

namespace Sgian.Protocol;

/// <summary>
/// A pane's keyboard lease (docs/design/keyboard-lease-and-ledger.md).
/// A null holder means the pane is unheld.
/// </summary>
public sealed record LeaseInfo
{
    [JsonPropertyName("holder")]
    public string? Holder { get; init; }

    [JsonPropertyName("since_ms")]
    public ulong? SinceMilliseconds { get; init; }

    public bool IsHeld => !string.IsNullOrEmpty(Holder);
}

/// <summary>
/// Pure keyboard-lease helpers shared by the view model and its tests.
/// </summary>
public static class LeaseState
{
    public const int HolderMaxLength = 64;

    /// <summary>Mirrors the daemon's holder rule: 1–64 printable ASCII characters, no whitespace.</summary>
    public static bool IsValidHolder(string value) =>
        !string.IsNullOrEmpty(value)
        && value.Length <= HolderMaxLength
        && value.All(character => character > ' ' && character < (char)0x7f);

    /// <summary>
    /// The label this client writes and takes leases as: SGIAN_HOLDER when set and
    /// valid, else user@host, the same default the daemon's ctl uses so the operator
    /// is one principal across surfaces.
    /// </summary>
    public static string DefaultHolder()
    {
        var configured = Environment.GetEnvironmentVariable("SGIAN_HOLDER");
        if (configured is not null && IsValidHolder(configured)) return configured;
        var user = Environment.GetEnvironmentVariable("USERNAME") ?? Environment.UserName;
        var host = (Environment.GetEnvironmentVariable("COMPUTERNAME") ?? Environment.MachineName).Split('.')[0];
        var label = $"{user}@{host}";
        return IsValidHolder(label) ? label : "operator";
    }

    /// <summary>
    /// Fold one <c>lease_state</c> event payload into the held-pane map: <c>taken</c>
    /// records the holder, <c>released</c>/<c>revoked</c> clear it. Returns the pane
    /// id, or null for a malformed payload.
    /// </summary>
    public static string? Apply(JsonElement payload, IDictionary<string, LeaseInfo> leases)
    {
        if (payload.ValueKind != JsonValueKind.Object) return null;
        if (!payload.TryGetProperty("pane_id", out var idElement) || idElement.ValueKind != JsonValueKind.String) return null;
        if (!payload.TryGetProperty("transition", out var transitionElement) || transitionElement.ValueKind != JsonValueKind.String) return null;
        var paneId = idElement.GetString()!;
        var holder = payload.TryGetProperty("holder", out var holderElement) && holderElement.ValueKind == JsonValueKind.String
            ? holderElement.GetString()
            : null;
        if (transitionElement.GetString() == "taken" && !string.IsNullOrEmpty(holder))
        {
            ulong? since = payload.TryGetProperty("since_ms", out var sinceElement) && sinceElement.TryGetUInt64(out var value)
                ? value
                : null;
            leases[paneId] = new LeaseInfo { Holder = holder, SinceMilliseconds = since };
        }
        else
        {
            leases.Remove(paneId);
        }
        return paneId;
    }

    public static bool IsRefusal(string message) => message.Contains("pane keyboard is", StringComparison.Ordinal);

    public static bool NeedsForce(string message) => message.Contains("--force", StringComparison.Ordinal);

    /// <summary>A pre-lease daemon rejects <c>send_input_as</c> with a serde "unknown variant" error.</summary>
    public static bool IsUnsupported(string message) => message.Contains("unknown variant", StringComparison.Ordinal);

    public static string NoticeText(string refusal)
    {
        const string marker = "held by ";
        var index = refusal.IndexOf(marker, StringComparison.Ordinal);
        if (index >= 0)
        {
            var rest = refusal[(index + marker.Length)..];
            var end = rest.IndexOfAny([' ', '(', ';']);
            var name = end < 0 ? rest : rest[..end];
            if (name.Length > 0) return $"Read-only: keyboard held by {name}. Ctrl+Shift+T to take it.";
        }
        if (refusal.Contains("unheld", StringComparison.Ordinal)) return "Read-only: take the keyboard (Ctrl+Shift+T) to type.";
        return refusal;
    }
}
