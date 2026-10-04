using System.Text.Json;
using System.Text.Json.Serialization;

namespace Sgian.Protocol;

/// <summary>Persisted layout shared with the daemon and both other clients.</summary>
public sealed record PaneLayout
{
    [JsonPropertyName("type")] public string Type { get; init; } = "leaf";
    [JsonPropertyName("id")] public required string Id { get; init; }
    [JsonPropertyName("direction")] public string? Direction { get; init; }
    [JsonPropertyName("ratio")] public double Ratio { get; init; } = 0.5;
    [JsonPropertyName("first")] public PaneLayout? First { get; init; }
    [JsonPropertyName("second")] public PaneLayout? Second { get; init; }
    [JsonIgnore] public bool IsLeaf => Type == "leaf";
    [JsonIgnore] public IReadOnlyList<string> PaneIds => IsLeaf ? [Id] : [.. First!.PaneIds, .. Second!.PaneIds];

    public static double Clamp(double ratio) => double.IsFinite(ratio) ? Math.Clamp(ratio, 0.18, 0.82) : 0.5;
    public static PaneLayout Leaf(string id) => new() { Id = id };
    public static PaneLayout Join(PaneLayout first, PaneLayout second, string direction, double ratio = 0.5) => new()
    {
        Type = "split", Id = $"split-{Guid.NewGuid()}", Direction = direction, Ratio = Clamp(ratio), First = first, Second = second,
    };

    public static PaneLayout? Parse(JsonElement? value, int depth = 0)
    {
        if (depth >= 32 || value is not { ValueKind: JsonValueKind.Object } node ||
            !node.TryGetProperty("id", out var id) || id.ValueKind != JsonValueKind.String ||
            string.IsNullOrWhiteSpace(id.GetString()) || !node.TryGetProperty("type", out var type)) return null;
        if (type.ValueKind != JsonValueKind.String) return null;
        if (type.GetString() == "leaf") return Leaf(id.GetString()!);
        if (type.GetString() != "split" || !node.TryGetProperty("direction", out var direction) ||
            direction.ValueKind != JsonValueKind.String || direction.GetString() is not ("row" or "column") ||
            !node.TryGetProperty("ratio", out var ratio) || ratio.ValueKind != JsonValueKind.Number ||
            !ratio.TryGetDouble(out var r) || !double.IsFinite(r) || r <= 0 || r >= 1 ||
            !node.TryGetProperty("first", out var a) || !node.TryGetProperty("second", out var b)) return null;
        var first = Parse(a, depth + 1);
        var second = Parse(b, depth + 1);
        if (first is null || second is null || first.PaneIds.Intersect(second.PaneIds).Any()) return null;
        return new PaneLayout { Type = "split", Id = id.GetString()!, Direction = direction.GetString(),
            Ratio = Clamp(r), First = first, Second = second };
    }

    public static PaneLayout? Reconcile(PaneLayout? tree, IEnumerable<string> paneIds)
    {
        var ids = paneIds.Distinct().ToArray();
        foreach (var stale in tree?.PaneIds.Except(ids).ToArray() ?? []) tree = tree?.Remove(stale);
        foreach (var id in ids)
            if (tree is null) tree = Leaf(id);
            // A pane this client did not place joins weighted by pane count so
            // every pane ends up the same width; a fixed half would halve
            // everything placed before it.
            else if (!tree.PaneIds.Contains(id)) tree = Join(tree, Leaf(id), "row", tree.PaneIds.Count / (tree.PaneIds.Count + 1.0));
        return tree;
    }

    public PaneLayout Insert(string id, string? anchor, string direction)
    {
        var tree = Remove(id);
        if (tree is null) return Leaf(id);
        return anchor is not null && tree.PaneIds.Contains(anchor)
            ? tree.Replace(anchor, Join(Leaf(anchor), Leaf(id), direction)) : Join(tree, Leaf(id), direction);
    }

    public PaneLayout Replace(string id, PaneLayout replacement) => IsLeaf ? (Id == id ? replacement : this)
        : this with { First = First!.Replace(id, replacement), Second = Second!.Replace(id, replacement) };

    public PaneLayout? Remove(string id)
    {
        if (IsLeaf) return Id == id ? null : this;
        var a = First!.Remove(id);
        var b = Second!.Remove(id);
        return a is null ? b : b is null ? a : this with { First = a, Second = b };
    }

    public PaneLayout Resize(string id, double ratio) => IsLeaf ? this : this with
    {
        Ratio = Id == id ? Clamp(ratio) : Ratio,
        First = First!.Resize(id, ratio), Second = Second!.Resize(id, ratio),
    };
}
