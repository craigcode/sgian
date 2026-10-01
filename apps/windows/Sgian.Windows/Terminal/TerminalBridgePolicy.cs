namespace Sgian.Windows.Terminal;

public static class TerminalBridgePolicy
{
    public const string DocumentUrl = "https://sgian.local/index.html";

    // Exact document identity also excludes userinfo, alternate ports,
    // query strings, frames, and other documents on the virtual host.
    public static bool IsTrustedDocument(string? source) =>
        string.Equals(source, DocumentUrl, StringComparison.Ordinal);
}
