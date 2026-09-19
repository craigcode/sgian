using System.IO.Pipes;
using System.Text;
using System.Text.Json;

namespace Sgian.Protocol;

public sealed class DaemonClient : IAsyncDisposable
{
    private static readonly JsonSerializerOptions JsonOptions = new(JsonSerializerDefaults.Web);
    private static readonly TimeSpan RequestTimeout = TimeSpan.FromSeconds(20);
    private readonly NativeIpcEndpoint _endpoint;
    private readonly string _token;
    private readonly string? _clientToken;
    private readonly SemaphoreSlim _requestGate = new(1, 1);
    private CancellationTokenSource? _subscriptionCancellation;

    private DaemonClient(NativeIpcEndpoint endpoint, string token, string? clientToken)
    {
        _endpoint = endpoint;
        _token = token;
        _clientToken = clientToken;
    }

    public NativeIpcEndpoint Endpoint => _endpoint;

    /// <summary>The holder the daemon bound this client to (from the hello's identity), or null for the workspace token.</summary>
    public string? IdentityHolder { get; private set; }

    /// <summary>(M6) The per-client credential this process presents, if any: SGIAN_CLIENT_TOKEN, else the first line of SGIAN_CLIENT_TOKEN_FILE.</summary>
    public static string? ClientTokenFromEnvironment()
    {
        var token = Environment.GetEnvironmentVariable("SGIAN_CLIENT_TOKEN")?.Trim();
        if (!string.IsNullOrEmpty(token)) return token;
        var path = Environment.GetEnvironmentVariable("SGIAN_CLIENT_TOKEN_FILE");
        if (string.IsNullOrEmpty(path) || !File.Exists(path)) return null;
        var first = File.ReadLines(path).FirstOrDefault()?.Trim();
        return string.IsNullOrEmpty(first) ? null : first;
    }

    public static async Task<DaemonClient> ConnectAsync(
        string workspace,
        string? backendPath = null,
        CancellationToken cancellationToken = default,
        Action<string>? onProgress = null)
    {
        var endpoint = await EndpointDiscovery.DiscoverAsync(
            workspace,
            backendPath,
            cancellationToken,
            onProgress).ConfigureAwait(false);
        onProgress?.Invoke("Reading daemon authentication token");
        var clientToken = ClientTokenFromEnvironment();
        var token = File.Exists(endpoint.TokenPath)
            ? (await File.ReadAllTextAsync(endpoint.TokenPath, cancellationToken).ConfigureAwait(false)).Trim()
            : "";
        // A remote client has no workspace token file; its credential rides the hello instead.
        if (token.Length == 0 && clientToken is null)
        {
            throw new DaemonProtocolException($"Daemon token is empty at {endpoint.TokenPath}.");
        }

        var client = new DaemonClient(endpoint, token, clientToken);
        onProgress?.Invoke("Sending initial daemon ping");
        var ping = await client.RequestAsync<CommandOk>(new Dictionary<string, object?>
        {
            ["command"] = "ping",
        }, cancellationToken, onProgress).ConfigureAwait(false);
        if (!ping.Ok)
        {
            await client.DisposeAsync().ConfigureAwait(false);
            throw new DaemonProtocolException("The daemon did not acknowledge the initial ping.");
        }
        onProgress?.Invoke("Initial daemon ping acknowledged");
        return client;
    }

    public async Task<T> RequestAsync<T>(
        IReadOnlyDictionary<string, object?> request,
        CancellationToken cancellationToken = default,
        Action<string>? onProgress = null)
    {
        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        timeout.CancelAfter(RequestTimeout);
        var operationToken = timeout.Token;
        await _requestGate.WaitAsync(operationToken).ConfigureAwait(false);
        try
        {
            await using var connection = await OpenAuthenticatedPipeAsync(operationToken, onProgress)
                .ConfigureAwait(false);
            await WriteLineAsync(connection.Writer, request, operationToken).ConfigureAwait(false);
            onProgress?.Invoke("Daemon request written; waiting for response");
            var line = await ReadLineAsync(connection.Reader, operationToken).ConfigureAwait(false);
            onProgress?.Invoke("Daemon response received");
            var response = JsonSerializer.Deserialize<IpcResponse>(line, JsonOptions)
                ?? throw new DaemonProtocolException("The daemon returned an empty response.");
            if (!response.Ok)
            {
                throw new DaemonProtocolException(response.Error ?? "The daemon request failed.");
            }
            return response.Result.Deserialize<T>(JsonOptions)
                ?? throw new DaemonProtocolException("The daemon returned an invalid result.");
        }
        catch (JsonException error)
        {
            throw new DaemonProtocolException("The daemon returned malformed JSON.", error);
        }
        catch (OperationCanceledException) when (!cancellationToken.IsCancellationRequested)
        {
            throw new DaemonProtocolException(
                $"The daemon request timed out after {RequestTimeout.TotalSeconds:0} seconds.");
        }
        finally
        {
            _requestGate.Release();
        }
    }

    public async Task SubscribeAsync(
        Func<DaemonEvent, Task> onEvent,
        Func<Task>? onReady = null,
        CancellationToken cancellationToken = default)
    {
        _subscriptionCancellation?.Cancel();
        _subscriptionCancellation?.Dispose();
        _subscriptionCancellation = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        var token = _subscriptionCancellation.Token;
        using var startup = CancellationTokenSource.CreateLinkedTokenSource(token);
        startup.CancelAfter(RequestTimeout);
        await using var connection = await OpenAuthenticatedPipeAsync(startup.Token).ConfigureAwait(false);
        await WriteLineAsync(connection.Writer, new Dictionary<string, object?>
        {
            ["command"] = "subscribe",
        }, startup.Token).ConfigureAwait(false);

        if (connection.SupportsSubscribeAck)
        {
            var acknowledgement = DaemonEvent.Parse(await ReadLineAsync(connection.Reader, startup.Token)
                .ConfigureAwait(false));
            if (!string.Equals(acknowledgement.Kind, "subscribe_ack", StringComparison.Ordinal))
            {
                throw new DaemonProtocolException(
                    $"Expected subscription acknowledgement, received {acknowledgement.Kind}.");
            }
        }
        if (onReady is not null)
        {
            await onReady().ConfigureAwait(false);
        }

        while (!token.IsCancellationRequested)
        {
            var line = await ReadLineAsync(connection.Reader, token).ConfigureAwait(false);
            await onEvent(DaemonEvent.Parse(line)).ConfigureAwait(false);
        }
    }

    public ValueTask DisposeAsync()
    {
        _subscriptionCancellation?.Cancel();
        _subscriptionCancellation?.Dispose();
        _subscriptionCancellation = null;
        _requestGate.Dispose();
        return ValueTask.CompletedTask;
    }

    private async Task<AuthenticatedPipe> OpenAuthenticatedPipeAsync(
        CancellationToken cancellationToken,
        Action<string>? onProgress = null)
    {
        if (!string.Equals(_endpoint.Transport, "named_pipe", StringComparison.Ordinal))
        {
            throw new DaemonProtocolException($"Expected a Windows named pipe, got {_endpoint.Transport}.");
        }

        var pipe = new NamedPipeClientStream(
            ".",
            EndpointDiscovery.PipeName(_endpoint.Endpoint),
            PipeDirection.InOut,
            PipeOptions.Asynchronous | PipeOptions.CurrentUserOnly);
        try
        {
            using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
            timeout.CancelAfter(TimeSpan.FromSeconds(5));
            onProgress?.Invoke("Connecting to daemon named pipe");
            await pipe.ConnectAsync(timeout.Token).ConfigureAwait(false);
            onProgress?.Invoke("Daemon named pipe connected");
            var reader = new BoundedLineReader(new StreamReader(pipe, new UTF8Encoding(false), false, 4096, leaveOpen: true));
            var writer = new StreamWriter(pipe, new UTF8Encoding(false), 4096, leaveOpen: true)
            {
                AutoFlush = true,
                NewLine = "\n",
            };
            var hello = new Dictionary<string, object?>
            {
                ["type"] = "hello",
                ["version"] = 1,
                ["token"] = _token,
                ["capabilities"] = new[] { "subscribe-ack" },
            };
            if (_clientToken is not null) hello["client_token"] = _clientToken;
            await WriteLineAsync(writer, hello, timeout.Token).ConfigureAwait(false);
            onProgress?.Invoke("Daemon hello written; waiting for authentication response");
            var line = await ReadLineAsync(reader, timeout.Token).ConfigureAwait(false);
            onProgress?.Invoke("Daemon authentication response received");
            var response = JsonSerializer.Deserialize<IpcResponse>(line, JsonOptions)
                ?? throw new DaemonProtocolException("The daemon returned an empty handshake.");
            if (!response.Ok)
            {
                throw new DaemonProtocolException(
                    $"Daemon authentication failed: {response.Error ?? "rejected"}");
            }
            if (response.Result.TryGetProperty("identity", out var identity) &&
                identity.ValueKind == JsonValueKind.Object &&
                identity.TryGetProperty("holder", out var boundHolder) &&
                boundHolder.ValueKind == JsonValueKind.String)
            {
                IdentityHolder = boundHolder.GetString();
            }
            var supportsAck = response.Result.TryGetProperty("capabilities", out var capabilities) &&
                capabilities.ValueKind == JsonValueKind.Array &&
                capabilities.EnumerateArray().Any(value => value.GetString() == "subscribe-ack");
            return new AuthenticatedPipe(pipe, reader, writer, supportsAck);
        }
        catch
        {
            await pipe.DisposeAsync().ConfigureAwait(false);
            throw;
        }
    }

    private static async Task WriteLineAsync(
        StreamWriter writer,
        object value,
        CancellationToken cancellationToken) =>
        await writer.WriteLineAsync(
            JsonSerializer.Serialize(value, JsonOptions).AsMemory(),
            cancellationToken).ConfigureAwait(false);

    private static async Task<string> ReadLineAsync(
        BoundedLineReader reader,
        CancellationToken cancellationToken) =>
        await reader.ReadLineAsync(cancellationToken).ConfigureAwait(false)
            ?? throw new DaemonProtocolException("The daemon closed the connection.");

    private sealed class AuthenticatedPipe : IAsyncDisposable
    {
        private readonly NamedPipeClientStream _pipe;

        public AuthenticatedPipe(
            NamedPipeClientStream pipe,
            BoundedLineReader reader,
            StreamWriter writer,
            bool supportsSubscribeAck)
        {
            _pipe = pipe;
            Reader = reader;
            Writer = writer;
            SupportsSubscribeAck = supportsSubscribeAck;
        }

        public BoundedLineReader Reader { get; }
        public StreamWriter Writer { get; }
        public bool SupportsSubscribeAck { get; }

        public async ValueTask DisposeAsync()
        {
            Writer.Dispose();
            Reader.Dispose();
            await _pipe.DisposeAsync().ConfigureAwait(false);
        }
    }
}
