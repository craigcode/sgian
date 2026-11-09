using Sgian.Protocol;
using Windows.Security.Credentials;

namespace Sgian.Windows;

/// <summary>
/// Per-workspace and per-machine client credentials (docs/design/client-identity.md)
/// in the Windows credential vault. The environment (SGIAN_CLIENT_TOKEN /
/// SGIAN_CLIENT_TOKEN_FILE) always wins so the SSH path keeps working.
/// </summary>
internal static class CredentialStore
{
    private const string Resource = "Sgian client credential";

    private static string? LoadAccount(string account)
    {
        try
        {
            var vault = new PasswordVault();
            var entry = vault.Retrieve(Resource, account);
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

    private static void SaveAccount(string account, string? token)
    {
        var vault = new PasswordVault();
        try
        {
            foreach (var existing in vault.FindAllByResource(Resource))
            {
                if (string.Equals(existing.UserName, account, StringComparison.OrdinalIgnoreCase))
                {
                    vault.Remove(existing);
                }
            }
        }
        catch (Exception) { /* no entries for the resource */ }
        var trimmed = token?.Trim();
        if (string.IsNullOrEmpty(trimmed)) return;
        vault.Add(new PasswordCredential(Resource, account, trimmed));
    }

    public static string? Load(string workspace) => LoadAccount(Path.GetFullPath(workspace));

    /// <summary>Store (replacing any previous entry) or, with null or blank, remove.</summary>
    public static void Save(string workspace, string? token) => SaveAccount(Path.GetFullPath(workspace), token);

    public static string? LoadMachine(string machineName, string? fallbackTokenPath = null)
    {
        var vaultToken = LoadAccount($"machine:{machineName}");
        if (!string.IsNullOrEmpty(vaultToken))
        {
            return vaultToken;
        }
        var path = fallbackTokenPath ?? EndpointDiscovery.DefaultMachineTokenPath(machineName);
        if (!File.Exists(path))
        {
            return null;
        }
        try
        {
            var first = File.ReadLines(path).FirstOrDefault()?.Trim();
            return string.IsNullOrEmpty(first) ? null : first;
        }
        catch (Exception)
        {
            return null;
        }
    }

    public static void SaveMachine(string machineName, string? token) =>
        SaveAccount($"machine:{machineName}", token);
}
