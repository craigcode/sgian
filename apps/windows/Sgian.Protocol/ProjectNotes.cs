using System.Text.Json;
using System.Text.Json.Serialization;

namespace Sgian.Protocol;

/// <summary>
/// One shared context note as the board shows it (docs/design/shared-context-notes.md):
/// who wrote it, when, and whether the output guard removed hidden text. The body never
/// reaches the board.
/// </summary>
public sealed record ProjectNote
{
    [JsonPropertyName("file")]
    public string File { get; init; } = "";

    [JsonPropertyName("title")]
    public string Title { get; init; } = "";

    /// <summary><c>daemon</c> when the daemon wrote it with its front matter, <c>file</c> otherwise.</summary>
    [JsonPropertyName("evidence")]
    public string Evidence { get; init; } = "file";

    [JsonPropertyName("holder")]
    public string? Holder { get; init; }

    [JsonPropertyName("pane")]
    public string? Pane { get; init; }

    [JsonPropertyName("written_at_ms")]
    public long? WrittenAtMs { get; init; }

    [JsonPropertyName("hash")]
    public string Hash { get; init; } = "";

    [JsonPropertyName("tricks")]
    public OutputTricks? Tricks { get; init; }

    /// <summary>The output guard removed hidden text from this note.</summary>
    public bool Guarded => Tricks is { Total: > 0 };

    /// <summary><c>YYYY-MM-DD</c> (UTC) for the daemon's timestamp, "–" when unknown.</summary>
    public string DateText => WrittenAtMs is { } ms
        ? DateTimeOffset.FromUnixTimeMilliseconds(ms).UtcDateTime.ToString("yyyy-MM-dd")
        : "–";

    /// <summary>"Flaky auth test — craig@mac · 2026-10-09 ⚠" (⚠ when the guard removed hidden text).</summary>
    public string Line => $"{Title} — {Holder ?? "file"} · {DateText}{(Guarded ? " ⚠" : "")}";
}

/// <summary>Pure helpers for the notes a project header shows.</summary>
public static class ProjectNotes
{
    /// <summary>
    /// Decode a <c>project_notes</c> listing (newest first), dropping malformed entries.
    /// Null when the payload is not a listing.
    /// </summary>
    public static IReadOnlyList<ProjectNote>? Parse(JsonElement listing)
    {
        if (listing.ValueKind != JsonValueKind.Object) return null;
        if (!listing.TryGetProperty("notes", out var entries) || entries.ValueKind != JsonValueKind.Array) return null;
        var notes = new List<ProjectNote>();
        foreach (var entry in entries.EnumerateArray())
        {
            if (entry.ValueKind != JsonValueKind.Object) continue;
            ProjectNote? note;
            try { note = entry.Deserialize<ProjectNote>(); }
            catch (JsonException) { continue; }
            if (note is null || string.IsNullOrEmpty(note.File)) continue;
            notes.Add(note with
            {
                Title = string.IsNullOrEmpty(note.Title) ? note.File : note.Title,
                Evidence = note.Evidence == "daemon" ? "daemon" : "file",
                Holder = string.IsNullOrEmpty(note.Holder) ? null : note.Holder,
            });
        }
        return notes;
    }

    /// <summary>
    /// The project a <c>project_notes_changed</c> payload names, or null when malformed. The
    /// event carries a file and a hash, never contents, so the client re-reads the listing.
    /// </summary>
    public static string? ChangedProject(JsonElement payload)
    {
        if (payload.ValueKind != JsonValueKind.Object) return null;
        if (!payload.TryGetProperty("project", out var project) || project.ValueKind != JsonValueKind.String) return null;
        var name = project.GetString();
        return string.IsNullOrEmpty(name) ? null : name;
    }

    /// <summary>
    /// What a project header shows: a summary ("2 notes · 1 hid text") then the newest
    /// <paramref name="limit"/> notes, one line each; empty when the project has none.
    /// </summary>
    public static IReadOnlyList<string> Lines(IReadOnlyList<ProjectNote> notes, int limit = 3)
    {
        if (notes.Count == 0) return [];
        var lines = new List<string> { $"{notes.Count} note{(notes.Count == 1 ? "" : "s")}" };
        var guarded = notes.Count(note => note.Guarded);
        if (guarded > 0) lines[0] += $" · {guarded} hid text";
        lines.AddRange(notes.Take(limit).Select(note => note.Line));
        return lines;
    }
}
