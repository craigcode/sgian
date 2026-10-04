namespace Sgian.Protocol;

/// <summary>
/// The workspace a client opens at launch. <c>SGIAN_WORKSPACE</c> wins even when
/// the directory is missing (an explicit request should fail visibly); the saved
/// path is used only while it still exists, then the most recent workspace that
/// does, then the current directory (the user profile when launched from a drive
/// root, as a Start-menu launch is).
/// </summary>
public static class WorkspaceStartup
{
    public static string Resolve(
        string? environment,
        string? saved,
        IReadOnlyList<string>? recent,
        string current,
        string? currentRoot,
        string profile,
        Func<string, bool> exists,
        IEnumerable<string?>? notAWorkspace = null)
    {
        if (!string.IsNullOrEmpty(environment)) return environment;
        if (!string.IsNullOrEmpty(saved) && exists(saved)) return saved;
        var alive = recent?.FirstOrDefault(path => !string.IsNullOrEmpty(path) && exists(path));
        if (alive is not null) return alive;
        // A launch from the Start menu or as a packaged app starts in a drive
        // root, the system directory or the install directory. None of those
        // is a project; open the user profile instead.
        var nowhere = string.Equals(current, currentRoot, StringComparison.OrdinalIgnoreCase)
            || (notAWorkspace ?? []).Any(path => !string.IsNullOrEmpty(path) && IsSameOrUnder(current, path));
        return nowhere ? profile : current;
    }

    private static bool IsSameOrUnder(string path, string parent)
    {
        var trimmedPath = path.TrimEnd('\\', '/');
        var trimmedParent = parent.TrimEnd('\\', '/');
        return string.Equals(trimmedPath, trimmedParent, StringComparison.OrdinalIgnoreCase)
            || trimmedPath.StartsWith(trimmedParent + "\\", StringComparison.OrdinalIgnoreCase)
            || trimmedPath.StartsWith(trimmedParent + "/", StringComparison.OrdinalIgnoreCase);
    }

    /// <summary>
    /// Whether a configured pane profile describes an agent, by the daemon's rule: an explicit
    /// <c>kind</c> wins, and with none an agent field makes it an agent profile.
    /// </summary>
    public static bool IsAgentProfile(string? kind, string? agentBackend, string? agentModel) =>
        kind == "agent" || (kind is null && (agentBackend is not null || agentModel is not null));
}
