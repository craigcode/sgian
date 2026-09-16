using System.Text.Json;

namespace Sgian.Protocol;

/// <summary>One section of the project board: a project (null name = panes in no project) and its member pane ids in project order.</summary>
public sealed record ProjectGroup(string? Name, string? Goal, IReadOnlyList<string> PaneIds)
{
    public string Title => Name ?? "No project";
}

/// <summary>What the roll-up needs to know about one pane.</summary>
public sealed record PaneFacts(string Id, string State, string? Attention, bool Unattended, string? Holder, bool HasWarning);

/// <summary>
/// The attention roll-up for a set of panes: what one glance needs to tell. Mirrors the
/// daemon's ProjectSummary so <c>ctl project list</c> and the sidebar agree.
/// </summary>
public sealed record ProjectRollup(
    int Panes, int Live, int NeedsInput, int Working, int Idle, int Unattended, int Held,
    IReadOnlyList<string> Holders, int Warnings)
{
    /// <summary>"3 panes · 1 needs input · 1 working · ⚠ 1 unattended · ⌨ alice": only the non-zero parts.</summary>
    public string Text
    {
        get
        {
            var parts = new List<string> { $"{Panes} pane{(Panes == 1 ? "" : "s")}" };
            if (Live != Panes) parts.Add($"{Live} live");
            if (NeedsInput > 0) parts.Add($"{NeedsInput} needs input");
            if (Working > 0) parts.Add($"{Working} working");
            if (Idle > 0) parts.Add($"{Idle} idle");
            if (Unattended > 0) parts.Add($"\u26A0 {Unattended} unattended");
            if (Warnings > 0) parts.Add($"{Warnings} with output warnings");
            if (Holders.Count > 0) parts.Add("\u2328 " + string.Join(", ", Holders));
            return string.Join(" \u00B7 ", parts);
        }
    }
}

/// <summary>Pure grouping and roll-up for the project board (ENHANCEMENTS "projects"); no view-model access so it is testable.</summary>
public static class ProjectBoard
{
    /// <summary>
    /// Projects sorted by name, member panes in project order; panes in no project last
    /// under a null name (omitted when every pane is assigned). A member id the client does
    /// not know is skipped: it closed between two events.
    /// </summary>
    public static IReadOnlyList<ProjectGroup> Group(IEnumerable<string> paneIds, IReadOnlyDictionary<string, Project> projects)
    {
        var known = paneIds.ToList();
        var knownSet = known.ToHashSet(StringComparer.Ordinal);
        var assigned = new HashSet<string>(StringComparer.Ordinal);
        var groups = new List<ProjectGroup>();
        foreach (var name in projects.Keys.OrderBy(name => name, StringComparer.Ordinal))
        {
            var project = projects[name];
            var members = new List<string>();
            foreach (var id in project.Panes)
            {
                if (!knownSet.Contains(id) || !assigned.Add(id)) continue;
                members.Add(id);
            }
            groups.Add(new ProjectGroup(name, string.IsNullOrEmpty(project.Goal) ? null : project.Goal, members));
        }
        var rest = known.Where(id => !assigned.Contains(id)).ToList();
        if (rest.Count > 0 || groups.Count == 0)
        {
            groups.Add(new ProjectGroup(null, null, rest));
        }
        return groups;
    }

    public static ProjectRollup Rollup(IEnumerable<PaneFacts> panes)
    {
        int count = 0, live = 0, needsInput = 0, working = 0, idle = 0, unattended = 0, held = 0, warnings = 0;
        var holders = new SortedSet<string>(StringComparer.Ordinal);
        foreach (var pane in panes)
        {
            count++;
            if (pane.State != "ended") live++;
            switch (pane.Attention)
            {
                case "needs_input": needsInput++; break;
                case "working": working++; break;
                case "idle": idle++; break;
            }
            if (pane.Unattended) unattended++;
            if (!string.IsNullOrEmpty(pane.Holder))
            {
                held++;
                holders.Add(pane.Holder);
            }
            if (pane.HasWarning) warnings++;
        }
        return new ProjectRollup(count, live, needsInput, working, idle, unattended, held, holders.ToList(), warnings);
    }

    /// <summary>The project a pane belongs to, or null.</summary>
    public static string? ProjectFor(string paneId, IReadOnlyDictionary<string, Project> projects) =>
        projects.OrderBy(entry => entry.Key, StringComparer.Ordinal)
            .FirstOrDefault(entry => entry.Value.Panes.Contains(paneId, StringComparer.Ordinal)).Key;

    /// <summary>
    /// Decode a <c>projects_changed</c> payload (or a snapshot's table): the daemon sends the
    /// whole table, so callers replace rather than diff. Null when malformed.
    /// </summary>
    public static IReadOnlyDictionary<string, Project>? ParseProjects(JsonElement payload)
    {
        if (payload.ValueKind != JsonValueKind.Object) return null;
        if (!payload.TryGetProperty("projects", out var table) || table.ValueKind != JsonValueKind.Object) return null;
        var projects = new Dictionary<string, Project>(StringComparer.Ordinal);
        foreach (var entry in table.EnumerateObject())
        {
            if (entry.Value.ValueKind != JsonValueKind.Object || string.IsNullOrEmpty(entry.Name)) continue;
            Project? project;
            try { project = entry.Value.Deserialize<Project>(); }
            catch (JsonException) { continue; }
            if (project is null) continue;
            var panes = project.Panes.Where(id => !string.IsNullOrEmpty(id)).Distinct(StringComparer.Ordinal).ToList();
            projects[entry.Name] = project with { Name = entry.Name, Panes = panes };
        }
        return projects;
    }

    /// <summary>
    /// Fold one <c>output_warning</c> payload into the per-pane map (its <c>total</c> is the
    /// running count). Returns the pane id, or null for a malformed payload.
    /// </summary>
    public static string? ApplyWarning(JsonElement payload, IDictionary<string, OutputTricks> warnings)
    {
        if (payload.ValueKind != JsonValueKind.Object) return null;
        if (!payload.TryGetProperty("pane_id", out var idElement) || idElement.ValueKind != JsonValueKind.String) return null;
        if (!payload.TryGetProperty("total", out var totalElement) || totalElement.ValueKind != JsonValueKind.Object) return null;
        OutputTricks? total;
        try { total = totalElement.Deserialize<OutputTricks>(); }
        catch (JsonException) { return null; }
        var paneId = idElement.GetString()!;
        if (paneId.Length == 0 || total is null) return null;
        if (total.Total > 0) warnings[paneId] = total;
        else warnings.Remove(paneId);
        return paneId;
    }
}
