using Windows.Security.Credentials;

namespace Sgian.Windows;

/// <summary>
/// Per-workspace client credentials (docs/design/client-identity.md) in the Windows
/// credential vault, one entry per workspace path. The environment
/// (SGIAN_CLIENT_TOKEN / SGIAN_CLIENT_TOKEN_FILE) always wins so the SSH path keeps working.
/// </summary>
internal static class CredentialStore
{
    private const string Resource = "Sgian client credential";

    public static string? Load(string workspace)
    {
        try
        {
            var vault = new PasswordVault();
            var entry = vault.Retrieve(Resource, Path.GetFullPath(workspace));
            entry.RetrievePassword();
            var token = entry.Password?.Trim();
            return string.IsNullOrEmpty(token) ? null : token;
        }
        catch (Exception)
        {
            // Retrieve throws when there is no entry; anything else means no credential either.
            return null;
        }
    }

    /// <summary>Store (replacing any previous entry) or, with null or blank, remove.</summary>
    public static void Save(string workspace, string? token)
    {
        var vault = new PasswordVault();
        var account = Path.GetFullPath(workspace);
        try
        {
            foreach (var existing in vault.FindAllByResource(Resource))
            {
                if (string.Equals(existing.UserName, account, StringComparison.OrdinalIgnoreCase)) vault.Remove(existing);
            }
        }
        catch (Exception) { /* no entries for the resource */ }
        var trimmed = token?.Trim();
        if (string.IsNullOrEmpty(trimmed)) return;
        vault.Add(new PasswordCredential(Resource, account, trimmed));
    }
}
