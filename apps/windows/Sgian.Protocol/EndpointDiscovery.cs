using System.Diagnostics;
using System.Net;
using System.Text;
using System.Text.Json;

namespace Sgian.Protocol;

public static class EndpointDiscovery
{
    private static readonly JsonSerializerOptions JsonOptions = new(JsonSerializerDefaults.Web);

    public static string ResolveAppDataRoot(string? appDataRoot = null)
    {
        if (!string.IsNullOrWhiteSpace(appDataRoot))
        {
            return appDataRoot.Trim();
        }
        var envOverride = Environment.GetEnvironmentVariable("SGIAN_MACHINES_HOME")?.Trim();
        if (!string.IsNullOrEmpty(envOverride))
        {
            return envOverride;
        }
        var roaming = Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData);
        var current = Path.Combine(roaming, "Sgian");
        var legacy = Path.Combine(roaming, "Sgian2");
        return Directory.Exists(current) || !Directory.Exists(legacy) ? current : legacy;
    }

    public static IReadOnlyList<SavedMachine> LoadSavedMachines(string? appDataRoot = null)
    {
        var configPath = Path.Combine(ResolveAppDataRoot(appDataRoot), "config.json");
        if (!File.Exists(configPath))
        {
            return [];
        }
        try
        {
            using var document = JsonDocument.Parse(File.ReadAllText(configPath));
            if (document.RootElement.ValueKind != JsonValueKind.Object ||
                !document.RootElement.TryGetProperty("machines", out var machinesElement) ||
                machinesElement.ValueKind != JsonValueKind.Array)
            {
                return [];
            }
            var parsed = machinesElement.Deserialize<SavedMachine[]>(JsonOptions) ?? [];
            return parsed.Where(machine => !string.IsNullOrWhiteSpace(machine.Name)).ToArray();
        }
        catch (Exception error) when (error is JsonException or IOException or UnauthorizedAccessException)
        {
            return [];
        }
    }

    public static string DefaultMachineTokenPath(
        string machineName,
        string? tokenFile = null,
        string? appDataRoot = null)
    {
        if (!string.IsNullOrWhiteSpace(tokenFile))
        {
            return tokenFile.Trim();
        }
        return Path.Combine(ResolveAppDataRoot(appDataRoot), "machines", $"{machineName}.token");
    }

    public static Task<NativeIpcEndpoint> DiscoverAsync(
        string workspace,
        string? backendPath = null,
        CancellationToken cancellationToken = default,
        Action<string>? onProgress = null) =>
        RunDiscoveryAsync(
            ["ctl", "--workspace", Path.GetFullPath(workspace), "--json", "ipc-endpoint"],
            clientToken: null,
            backendPath: backendPath,
            requireTokenPath: true,
            cancellationToken: cancellationToken,
            onProgress: onProgress);

    public static Task<NativeIpcEndpoint> DiscoverMachineAsync(
        string machineName,
        string? clientToken = null,
        string? backendPath = null,
        CancellationToken cancellationToken = default,
        Action<string>? onProgress = null) =>
        RunDiscoveryAsync(
            ["ctl", "--machine", machineName, "--json", "ipc-endpoint"],
            clientToken: clientToken,
            backendPath: backendPath,
            requireTokenPath: false,
            cancellationToken: cancellationToken,
            onProgress: onProgress);

    private static async Task<NativeIpcEndpoint> RunDiscoveryAsync(
        IReadOnlyList<string> arguments,
        string? clientToken,
        string? backendPath,
        bool requireTokenPath,
        CancellationToken cancellationToken,
        Action<string>? onProgress)
    {
        var executable = LocateBackend(backendPath);
        onProgress?.Invoke($"Starting endpoint discovery with {executable}");
        var start = new ProcessStartInfo
        {
            FileName = executable,
            UseShellExecute = false,
            CreateNoWindow = true,
            RedirectStandardOutput = true,
            // The detached Windows daemon can keep a redirected stderr pipe
            // open after this short-lived ctl helper has exited. Discovery is
            // a one-line JSON protocol, so do not wait on that inherited pipe.
            RedirectStandardError = false,
        };
        foreach (var argument in arguments)
        {
            start.ArgumentList.Add(argument);
        }
        if (!string.IsNullOrWhiteSpace(clientToken))
        {
            start.Environment["SGIAN_CLIENT_TOKEN"] = clientToken.Trim();
        }

        using var process = Process.Start(start)
            ?? throw new DaemonProtocolException("Could not start the Sgian backend.");
        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        timeout.CancelAfter(TimeSpan.FromSeconds(30));
        string output;
        var terminatedAfterResponse = false;
        try
        {
            // The backend's shared JSON writer emits indented, multi-line JSON,
            // while a detached descendant can keep stdout open beyond the ctl
            // response. Stop as soon as one complete JSON document is readable;
            // EOF is not the message boundary on Windows.
            output = await ReadJsonDocumentAsync(process.StandardOutput, timeout.Token)
                .ConfigureAwait(false);
            onProgress?.Invoke("Endpoint metadata received; waiting for discovery helper to exit");
            using var exitGrace = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
            exitGrace.CancelAfter(TimeSpan.FromSeconds(2));
            try
            {
                await process.WaitForExitAsync(exitGrace.Token).ConfigureAwait(false);
            }
            catch (OperationCanceledException) when (!cancellationToken.IsCancellationRequested)
            {
                // The Windows GUI-subsystem backend can remain alive after its
                // one-line ctl response has been fully delivered. The daemon it
                // spawned is an independent process with null stdio, so terminate
                // only this completed ctl helper (never its process tree).
                onProgress?.Invoke("Discovery helper remained alive after its response; terminating helper");
                terminatedAfterResponse = true;
                if (!process.HasExited)
                {
                    process.Kill();
                }
                await process.WaitForExitAsync(timeout.Token).ConfigureAwait(false);
            }
        }
        catch (OperationCanceledException)
        {
            if (!process.HasExited)
            {
                process.Kill();
            }
            cancellationToken.ThrowIfCancellationRequested();
            throw new DaemonProtocolException("Sgian backend discovery timed out after 30 seconds.");
        }
        catch
        {
            if (!process.HasExited) process.Kill();
            throw;
        }
        onProgress?.Invoke($"Endpoint discovery helper exited with code {process.ExitCode}");
        if (process.ExitCode != 0 && !terminatedAfterResponse)
        {
            throw new DaemonProtocolException(
                $"Sgian backend discovery failed with exit {process.ExitCode}.");
        }

        try
        {
            var endpoint = JsonSerializer.Deserialize<NativeIpcEndpoint>(output, JsonOptions)
                ?? throw new DaemonProtocolException("The Sgian backend returned no endpoint.");
            if (endpoint.ProtocolVersion != 1 || endpoint.Endpoint.Length == 0 ||
                (requireTokenPath && endpoint.TokenPath.Length == 0))
            {
                throw new DaemonProtocolException("The Sgian backend returned incomplete endpoint metadata.");
            }
            onProgress?.Invoke($"Discovered {endpoint.Transport} endpoint");
            return endpoint;
        }
        catch (JsonException error)
        {
            throw new DaemonProtocolException("The Sgian backend returned invalid endpoint JSON.", error);
        }
    }

    public static string PipeName(string endpoint)
    {
        const string prefix = @"\\.\pipe\";
        if (!endpoint.StartsWith(prefix, StringComparison.OrdinalIgnoreCase))
        {
            throw new DaemonProtocolException($"Unsupported Windows named-pipe endpoint: {endpoint}");
        }
        var name = endpoint[prefix.Length..];
        return name.Length > 0
            ? name
            : throw new DaemonProtocolException("The Windows named-pipe endpoint is empty.");
    }

    public static IPEndPoint ParseLoopbackEndpoint(string endpoint)
    {
        if (!IPEndPoint.TryParse(endpoint, out var parsed) ||
            parsed.Port <= 0 ||
            !IPAddress.IsLoopback(parsed.Address))
        {
            throw new DaemonProtocolException($"Refusing non-loopback or invalid TCP endpoint: {endpoint}");
        }
        return parsed;
    }

    private static async Task<string> ReadJsonDocumentAsync(
        StreamReader reader,
        CancellationToken cancellationToken)
    {
        const int maximumCharacters = 1024 * 1024;
        var json = new StringBuilder();
        using var bounded = new BoundedLineReader(reader, maximumCharacters, leaveOpen: true);
        while (await bounded.ReadLineAsync(cancellationToken).ConfigureAwait(false) is { } line)
        {
            json.AppendLine(line);
            if (json.Length > maximumCharacters)
            {
                throw new DaemonProtocolException("The Sgian backend endpoint metadata is too large.");
            }
            try
            {
                using var document = JsonDocument.Parse(json.ToString());
                return json.ToString();
            }
            catch (JsonException)
            {
                // Pretty-printed JSON is incomplete until its closing line.
            }
        }
        throw new DaemonProtocolException("The Sgian backend returned incomplete endpoint JSON.");
    }

    private static string LocateBackend(string? configured)
    {
        var candidates = new List<string?>
        {
            configured,
            Environment.GetEnvironmentVariable("SGIAN_BACKEND_BINARY"),
            Path.Combine(AppContext.BaseDirectory, "Helpers", "sgian.exe"),
#if DEBUG
            Path.Combine(Environment.CurrentDirectory, "src-tauri", "target", "release", "sgian.exe"),
            Path.Combine(Environment.CurrentDirectory, "src-tauri", "target", "debug", "sgian.exe"),
#endif
        };
        var path = Environment.GetEnvironmentVariable("PATH") ?? "";
        candidates.AddRange(path.Split(Path.PathSeparator).Select(folder => Path.Combine(folder, "sgian.exe")));
        return candidates.FirstOrDefault(candidate => !string.IsNullOrWhiteSpace(candidate) && File.Exists(candidate))
            ?? throw new DaemonProtocolException(
                "The Sgian backend could not be found. Set SGIAN_BACKEND_BINARY or use a packaged build.");
    }
}
