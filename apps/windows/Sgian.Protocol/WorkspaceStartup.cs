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
        Func<string, bool> exists)
    {
        if (!string.IsNullOrEmpty(environment)) return environment;
        if (!string.IsNullOrEmpty(saved) && exists(saved)) return saved;
        var alive = recent?.FirstOrDefault(path => !string.IsNullOrEmpty(path) && exists(path));
        if (alive is not null) return alive;
        return string.Equals(current, currentRoot, StringComparison.OrdinalIgnoreCase) ? profile : current;
    }
}
