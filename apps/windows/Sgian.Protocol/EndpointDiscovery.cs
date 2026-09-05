using System.Diagnostics;
using System.Text;
using System.Text.Json;

namespace Sgian.Protocol;

public static class EndpointDiscovery
{
    private static readonly JsonSerializerOptions JsonOptions = new(JsonSerializerDefaults.Web);

    public static async Task<NativeIpcEndpoint> DiscoverAsync(
        string workspace,
        string? backendPath = null,
        CancellationToken cancellationToken = default,
        Action<string>? onProgress = null)
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
        start.ArgumentList.Add("ctl");
        start.ArgumentList.Add("--workspace");
        start.ArgumentList.Add(Path.GetFullPath(workspace));
        start.ArgumentList.Add("--json");
        start.ArgumentList.Add("ipc-endpoint");

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
            if (endpoint.ProtocolVersion != 1 || endpoint.Endpoint.Length == 0 || endpoint.TokenPath.Length == 0)
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
            Path.Combine(Environment.CurrentDirectory, "src-tauri", "target", "release", "sgian.exe"),
            Path.Combine(Environment.CurrentDirectory, "src-tauri", "target", "debug", "sgian.exe"),
        };
        var path = Environment.GetEnvironmentVariable("PATH") ?? "";
        candidates.AddRange(path.Split(Path.PathSeparator).Select(folder => Path.Combine(folder, "sgian.exe")));
        return candidates.FirstOrDefault(candidate => !string.IsNullOrWhiteSpace(candidate) && File.Exists(candidate))
            ?? throw new DaemonProtocolException(
                "The Sgian backend could not be found. Set SGIAN_BACKEND_BINARY or use a packaged build.");
    }
}
