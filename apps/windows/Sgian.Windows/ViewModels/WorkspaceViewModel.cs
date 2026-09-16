using System.Collections.ObjectModel;
using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.UI.Dispatching;
using Sgian.Protocol;

namespace Sgian.Windows.ViewModels;

public sealed class WorkspaceViewModel : ObservableObject, IAsyncDisposable
{
    private readonly DispatcherQueue _dispatcher;
    private readonly Dictionary<string, AgentChatState> _chats = [];
    private readonly Dictionary<string, string> _scrollback = [];
    private readonly Dictionary<string, PaneSize> _sizes = [];
    /// <summary>Projects by name (ENHANCEMENTS "projects"); the sidebar shows them.</summary>
    private Dictionary<string, Project> _projects = new(StringComparer.Ordinal);
    /// <summary>Panes whose output hid something (docs/design/keyboard-lease-and-ledger.md §7).</summary>
    private readonly Dictionary<string, OutputTricks> _outputWarnings = new(StringComparer.Ordinal);
    private DaemonClient? _client;
    private CancellationTokenSource? _connectionCancellation;
    private PaneViewModel? _selectedPane;
    private string _status = "Disconnected";
    private string? _errorMessage;
    private string _workspacePath;
    private double _terminalFontSize;
    private Guid _generation;
    private CancellationTokenSource? _layoutSaveCancellation;
    private string? _leaseNotice;
    private CancellationTokenSource? _leaseNoticeCancellation;
    /// <summary>
    /// False once the daemon rejected <c>send_input_as</c> (a pre-lease daemon);
    /// input then falls back to the unattributed <c>write_to_pane</c>.
    /// </summary>
    private bool _daemonSupportsLeases = true;
    /// <summary>The holder label this client writes and takes leases as (docs/design/keyboard-lease-and-ledger.md).</summary>
    public string Holder { get; } = LeaseState.DefaultHolder();
    public PaneLayout? Layout { get; private set; }
    public bool Zoomed { get; private set; }
    public string PermissionMode { get; private set; } = "manual";
    public IReadOnlyList<string> RecentWorkspaces { get; private set; }
    public Guid Generation => _generation;
    public event EventHandler? WorkspaceReset;
    public event EventHandler? LayoutRestored;

    public WorkspaceViewModel(DispatcherQueue dispatcher)
    {
        _dispatcher = dispatcher;
        var settings = AppSettings.Load();
        RecentWorkspaces = settings.RecentWorkspaces ?? [];
        var environment = Environment.GetEnvironmentVariable("SGIAN_WORKSPACE");
        var fallback = Environment.CurrentDirectory == Path.GetPathRoot(Environment.CurrentDirectory)
            ? Environment.GetFolderPath(Environment.SpecialFolder.UserProfile)
            : Environment.CurrentDirectory;
        _workspacePath = environment ?? settings.WorkspacePath ?? fallback;
        _terminalFontSize = settings.TerminalFontSize is >= 9 and <= 30
            ? settings.TerminalFontSize
            : 13;
    }

    public ObservableCollection<PaneViewModel> Panes { get; } = [];
    public IReadOnlyDictionary<string, Project> Projects => _projects;

    /// <summary>
    /// One line per project with its attention roll-up ("feat: 2 panes · 1 needs input · ⌨ alice"),
    /// projects sorted by name, unassigned panes last; empty when there are no projects.
    /// </summary>
    public string ProjectSummary
    {
        get
        {
            if (_projects.Count == 0) return "";
            var facts = Panes.ToDictionary(
                pane => pane.Id,
                pane => new PaneFacts(pane.Id, pane.State, pane.Attention, pane.Unattended, pane.LeaseHolder, pane.OutputWarning is not null),
                StringComparer.Ordinal);
            var lines = ProjectBoard.Group(Panes.Select(pane => pane.Id), _projects)
                .Select(group => $"{group.Title}: {ProjectBoard.Rollup(group.PaneIds.Select(id => facts[id])).Text}");
            return string.Join("\n", lines);
        }
    }

    public event EventHandler? WorkspaceChanged;
    public event EventHandler<ChatChangedEventArgs>? ChatChanged;
    public event EventHandler<TerminalOutputEventArgs>? TerminalOutput;
    public event EventHandler? TerminalSettingsChanged;

    public PaneViewModel? SelectedPane
    {
        get => _selectedPane;
        private set => Set(ref _selectedPane, value);
    }

    public string Status
    {
        get => _status;
        private set => Set(ref _status, value);
    }

    /// <summary>A transient "read-only: held by …" notice after a refused keystroke.</summary>
    public string? LeaseNotice
    {
        get => _leaseNotice;
        private set => Set(ref _leaseNotice, value);
    }

    public string? ErrorMessage
    {
        get => _errorMessage;
        private set => Set(ref _errorMessage, value);
    }

    public string WorkspacePath
    {
        get => _workspacePath;
        private set => Set(ref _workspacePath, value);
    }

    public double TerminalFontSize
    {
        get => _terminalFontSize;
        set
        {
            var clamped = Math.Clamp(value, 9, 30);
            if (Set(ref _terminalFontSize, clamped))
            {
                SaveSettings();
                TerminalSettingsChanged?.Invoke(this, EventArgs.Empty);
            }
        }
    }

    public async Task StartAsync() => await ConnectAsync(WorkspacePath);

    public async Task ConnectAsync(string workspace)
    {
        var fullPath = Path.GetFullPath(workspace);
        if (!Directory.Exists(fullPath))
        {
            ErrorMessage = $"Workspace does not exist: {fullPath}";
            Status = "Connection failed";
            return;
        }

        _connectionCancellation?.Cancel();
        _layoutSaveCancellation?.Cancel();
        _connectionCancellation?.Dispose();
        _connectionCancellation = new CancellationTokenSource();
        var token = _connectionCancellation.Token;
        var generation = _generation = Guid.NewGuid();
        if (_client is not null)
        {
            var previous = _client;
            _client = null;
            await previous.DisposeAsync();
            if (generation != _generation) return;
        }

        WorkspacePath = fullPath;
        RecentWorkspaces = new[] { fullPath }.Concat(RecentWorkspaces)
            .Distinct(StringComparer.OrdinalIgnoreCase).Take(10).ToArray();
        SaveSettings();
        ErrorMessage = null;
        Status = "Connecting";
        Panes.Clear();
        _chats.Clear();
        _scrollback.Clear();
        _sizes.Clear();
        SelectedPane = null;
        Layout = null;
        Zoomed = false;
        WorkspaceReset?.Invoke(this, EventArgs.Empty);
        WorkspaceChanged?.Invoke(this, EventArgs.Empty);

        try
        {
            App.TraceSmoke("Starting daemon discovery and authentication");
            var client = await DaemonClient.ConnectAsync(
                fullPath,
                cancellationToken: token,
                onProgress: App.TraceSmoke);
            if (_generation != generation)
            {
                await client.DisposeAsync();
                return;
            }
            _client = client;
            App.TraceSmoke("Daemon client connected; requesting workspace bootstrap");
            var snapshot = await BootstrapAsync(client, token);
            if (_generation != generation) return;
            App.TraceSmoke($"Daemon bootstrap returned {snapshot.Panes.Count} pane(s)");
            Apply(snapshot, resetTerminals: true);
            Status = "Connected";
            App.TraceSmoke($"Workspace connected; selected pane is {SelectedPane?.Id ?? "none"}");
            _ = SubscribeLoopAsync(client, generation, token);
            await RefreshConfigurationAsync();
        }
        catch (OperationCanceledException) when (token.IsCancellationRequested)
        {
        }
        catch (Exception error)
        {
            if (_generation != generation) return;
            Fail(error);
        }
    }

    public AgentChatState ChatFor(string paneId)
    {
        if (!_chats.TryGetValue(paneId, out var chat))
        {
            chat = new AgentChatState();
            chat.Changed += (_, _) =>
            {
                var pane = Panes.FirstOrDefault(item => item.Id == paneId);
                if (pane is not null) pane.Attention = chat.PendingPermission is not null ? "needs_input" : chat.Busy ? "working" : "idle";
                ChatChanged?.Invoke(this, new ChatChangedEventArgs(paneId));
            };
            _chats[paneId] = chat;
        }
        return chat;
    }

    public string InitialScrollback(string paneId) =>
        _scrollback.TryGetValue(paneId, out var value) ? value : "";
    public PaneSize? InitialSize(string paneId) => _sizes.GetValueOrDefault(paneId);

    public async Task SelectAsync(PaneViewModel? pane)
    {
        SelectedPane = pane;
        WorkspaceChanged?.Invoke(this, EventArgs.Empty);
        if (pane is null || _client is null)
        {
            return;
        }
        await RunRequestAsync(() => _client.RequestAsync<CommandOk>(Request(
            ("command", "set_active_pane"), ("pane_id", pane.Id))));
    }

    public async Task CreateShellAsync(string direction = "row", string? profile = null)
    {
        if (_client is null) return;
        var anchor = SelectedPane?.Id;
        var pane = await RunRequestAsync(() => _client.RequestAsync<Pane>(Request(
            ("command", "create_pane"), ("title", null), ("profile", profile))));
        if (pane is not null)
        {
            var item = Upsert(pane);
            Layout = Layout?.Insert(pane.Id, anchor, direction) ?? PaneLayout.Leaf(pane.Id);
            Zoomed = false;
            SaveLayout();
            await SelectAsync(item);
        }
    }

    public async Task CreateAgentAsync(string backend, string? model = null)
    {
        if (_client is null) return;
        var pane = await RunRequestAsync(() => _client.RequestAsync<Pane>(Request(
            ("command", "create_agent_pane_with_spec"), ("title", null),
            ("backend", backend), ("model", model))));
        if (pane is not null)
        {
            var item = Upsert(pane);
            item.AgentSpec = new AgentPaneSpec { Backend = backend, Model = model };
            SaveLayout();
            ChatFor(pane.Id);
            await SelectAsync(item);
        }
    }

    public async Task CloseAsync(PaneViewModel pane)
    {
        if (_client is null) return;
        var snapshot = await RunRequestAsync(() => _client.RequestAsync<WorkspaceSnapshot>(Request(
            ("command", "close_pane"), ("pane_id", pane.Id))));
        if (snapshot is not null) { Apply(snapshot, resetTerminals: false); SaveLayout(); }
    }

    public async Task RenameAsync(PaneViewModel pane, string title)
    {
        if (_client is null || string.IsNullOrWhiteSpace(title)) return;
        var renamed = await RunRequestAsync(() => _client.RequestAsync<Pane>(Request(
            ("command", "rename_pane"), ("pane_id", pane.Id), ("title", title.Trim()))));
        if (renamed is not null) Upsert(renamed);
    }

    public async Task RestartAsync(PaneViewModel pane)
    {
        if (_client is null) return;
        var result = await RunRequestAsync(() => _client.RequestAsync<CommandOk>(Request(
            ("command", "restart_pane_terminal"), ("pane_id", pane.Id))));
        if (result is not null) { pane.State = "live"; WorkspaceChanged?.Invoke(this, EventArgs.Empty); }
    }

    public async Task EnsureTerminalAsync(string paneId)
    {
        if (Panes.FirstOrDefault(pane => pane.Id == paneId)?.State == "ended") return;
        if (_client is null) return;
        await RunRequestAsync(() => _client.RequestAsync<CommandOk>(Request(
            ("command", "ensure_pane_terminal"), ("pane_id", paneId))), showError: false);
    }

    public async Task WriteTerminalAsync(string paneId, string data)
    {
        if (_client is null || data.Length == 0) return;
        var generation = _generation;
        try
        {
            await SendInputAsync(paneId, data);
        }
        catch (Exception error) when (generation == _generation)
        {
            // A lease refusal is per keystroke and expected: a transient notice,
            // not the error bar (docs/design/keyboard-lease-and-ledger.md §6 M2).
            if (LeaseState.IsRefusal(error.Message)) ShowLeaseNotice(LeaseState.NoticeText(error.Message));
            else ErrorMessage = error.Message;
        }
    }

    /// <summary>Attributed input, falling back to the unattributed write against a pre-lease daemon.</summary>
    private async Task SendInputAsync(string paneId, string data)
    {
        if (_client is null) return;
        if (_daemonSupportsLeases)
        {
            try
            {
                await _client.RequestAsync<CommandOk>(Request(
                    ("command", "send_input_as"), ("pane_id", paneId), ("input", data), ("holder", Holder)));
                return;
            }
            catch (Exception error) when (LeaseState.IsUnsupported(error.Message))
            {
                _daemonSupportsLeases = false;
            }
        }
        await _client.RequestAsync<CommandOk>(Request(
            ("command", "write_to_pane"), ("pane_id", paneId), ("data", data)));
    }

    private void ShowLeaseNotice(string message)
    {
        LeaseNotice = message;
        _leaseNoticeCancellation?.Cancel();
        var cancellation = new CancellationTokenSource();
        _leaseNoticeCancellation = cancellation;
        _ = ClearLeaseNoticeLaterAsync(cancellation.Token);
    }

    private async Task ClearLeaseNoticeLaterAsync(CancellationToken token)
    {
        try
        {
            await Task.Delay(TimeSpan.FromSeconds(3), token);
            LeaseNotice = null;
        }
        catch (OperationCanceledException)
        {
        }
    }

    public enum LeaseOutcome { Applied, NeedsForce, Failed }

    /// <summary>
    /// Take a pane's keyboard. Unheld or already ours, the daemon answers at once;
    /// held by someone else, it refuses without force and the caller asks for a why.
    /// </summary>
    public async Task<LeaseOutcome> TakeLeaseAsync(PaneViewModel pane, bool force = false, string? why = null)
    {
        if (_client is null) return LeaseOutcome.Failed;
        var generation = _generation;
        try
        {
            var request = Request(("command", "take_lease"), ("pane_id", pane.Id), ("holder", Holder), ("force", force));
            if (why is not null) request["why"] = why;
            var info = await _client.RequestAsync<LeaseInfo>(request);
            if (generation != _generation) return LeaseOutcome.Failed;
            ApplyLease(pane, info);
            return LeaseOutcome.Applied;
        }
        catch (Exception error)
        {
            if (generation != _generation) return LeaseOutcome.Failed;
            if (!force && LeaseState.NeedsForce(error.Message)) return LeaseOutcome.NeedsForce;
            ErrorMessage = error.Message;
            return LeaseOutcome.Failed;
        }
    }

    public async Task<bool> ReleaseLeaseAsync(PaneViewModel pane, string note)
    {
        if (_client is null) return false;
        var generation = _generation;
        try
        {
            var info = await _client.RequestAsync<LeaseInfo>(Request(
                ("command", "release_lease"), ("pane_id", pane.Id), ("holder", Holder), ("note", note)));
            if (generation != _generation) return false;
            ApplyLease(pane, info);
            return true;
        }
        catch (Exception error)
        {
            if (generation == _generation) ErrorMessage = error.Message;
            return false;
        }
    }

    private void ApplyLease(PaneViewModel pane, LeaseInfo? info)
    {
        pane.LeaseHolder = info?.IsHeld == true ? info.Holder : null;
        pane.LeaseIsMine = pane.LeaseHolder is not null && pane.LeaseHolder == Holder;
    }

    public async Task ResizeTerminalAsync(string paneId, ushort columns, ushort rows)
    {
        if (_client is null) return;
        await RunRequestAsync(() => _client.RequestAsync<CommandOk>(Request(
            ("command", "resize_pane_terminal"), ("pane_id", paneId),
            ("cols", columns), ("rows", rows))), showError: false);
    }

    public async Task SendAgentMessageAsync(PaneViewModel pane, string text)
    {
        if (_client is null || string.IsNullOrWhiteSpace(text)) return;
        var chat = ChatFor(pane.Id);
        var body = text.Trim();
        var messageId = Guid.NewGuid().ToString();
        chat.AppendUserMessage(body, messageId);
        var result = await RunRequestAsync(() => _client.RequestAsync<CommandOk>(Request(
            ("command", "send_agent_message"), ("pane_id", pane.Id), ("text", body), ("message_id", messageId))));
        if (result is null) chat.RemoveLastUserMessage(body, messageId);
    }

    public async Task InterruptAgentAsync(PaneViewModel pane)
    {
        if (_client is null) return;
        await RunRequestAsync(() => _client.RequestAsync<CommandOk>(Request(
            ("command", "interrupt_agent"), ("pane_id", pane.Id))));
    }

    public async Task ResolvePermissionAsync(PaneViewModel pane, bool allow, string? message = null)
    {
        if (_client is null) return;
        var permission = ChatFor(pane.Id).PendingPermission;
        if (permission is null) return;
        await RunRequestAsync(() => _client.RequestAsync<CommandOk>(Request(
            ("command", "agent_approval"), ("pane_id", pane.Id),
            ("request_id", permission.RequestId), ("allow", allow), ("message", message))));
    }

    public async ValueTask DisposeAsync()
    {
        _connectionCancellation?.Cancel();
        _layoutSaveCancellation?.Cancel();
        _connectionCancellation?.Dispose();
        if (_client is not null) await _client.DisposeAsync();
    }

    private async Task SubscribeLoopAsync(DaemonClient initialClient, Guid generation, CancellationToken token)
    {
        var client = initialClient;
        var attempt = 0;
        while (!token.IsCancellationRequested && _generation == generation)
        {
            try
            {
                await client.SubscribeAsync(
                    onEvent: item =>
                    {
                        _dispatcher.TryEnqueue(() => { if (_generation == generation) Apply(item); });
                        return Task.CompletedTask;
                    },
                    onReady: async () =>
                    {
                        var snapshot = await BootstrapAsync(client, token);
                        _dispatcher.TryEnqueue(() =>
                        {
                            if (_generation != generation) return;
                            Apply(snapshot, resetTerminals: true);
                            Status = "Connected";
                            attempt = 0;
                            ErrorMessage = null;
                        });
                    },
                    cancellationToken: token);
            }
            catch (OperationCanceledException) when (token.IsCancellationRequested)
            {
                return;
            }
            catch (Exception error)
            {
                _dispatcher.TryEnqueue(() =>
                {
                    if (_generation != generation) return;
                    Status = "Reconnecting";
                    ErrorMessage = error.Message;
                });
            }

            var delay = TimeSpan.FromMilliseconds(Math.Min(750 * (1 << Math.Min(attempt++, 4)), 10000));
            try
            {
                await Task.Delay(delay, token);
                await client.DisposeAsync();
                client = await DaemonClient.ConnectAsync(WorkspacePath, cancellationToken: token);
                if (_generation != generation)
                {
                    await client.DisposeAsync();
                    return;
                }
                _client = client;
            }
            catch (OperationCanceledException) when (token.IsCancellationRequested)
            {
                return;
            }
            catch
            {
                continue;
            }
        }
    }

    private static Task<WorkspaceSnapshot> BootstrapAsync(DaemonClient client, CancellationToken token) =>
        client.RequestAsync<WorkspaceSnapshot>(Request(("command", "bootstrap_workspace")), token);

    private void Apply(WorkspaceSnapshot snapshot, bool resetTerminals)
    {
        var previousSelection = SelectedPane?.Id;
        var liveIds = snapshot.Panes.Select(pane => pane.Id).ToHashSet(StringComparer.Ordinal);
        foreach (var stale in Panes.Where(pane => !liveIds.Contains(pane.Id)).ToList())
        {
            Panes.Remove(stale);
            _chats.Remove(stale.Id);
            _scrollback.Remove(stale.Id);
            _sizes.Remove(stale.Id);
            _outputWarnings.Remove(stale.Id);
        }
        _projects = new Dictionary<string, Project>(snapshot.Projects, StringComparer.Ordinal);
        _outputWarnings.Clear();
        foreach (var (warnedPaneId, warning) in snapshot.OutputWarnings)
        {
            if (warning.Total > 0) _outputWarnings[warnedPaneId] = warning;
        }
        foreach (var pane in snapshot.Panes)
        {
            var item = Upsert(pane);
            item.ProjectName = ProjectBoard.ProjectFor(pane.Id, _projects);
            item.OutputWarning = _outputWarnings.GetValueOrDefault(pane.Id);
            item.State = snapshot.PaneStates.TryGetValue(pane.Id, out var state) ? state : "live";
            item.Attention = snapshot.AgentStates.TryGetValue(pane.Id, out var info)
                ? info.Attention
                : null;
            item.Mode = info?.Mode;
            item.Unattended = info?.Unattended ?? false;
            item.AgentSpec = snapshot.AgentSpecs.TryGetValue(pane.Id, out var spec) ? spec : null;
            ApplyLease(item, snapshot.Leases.TryGetValue(pane.Id, out var lease) ? lease : null);
            if (pane.Kind == "agent")
            {
                var chat = ChatFor(pane.Id);
                if (snapshot.AgentEvents.TryGetValue(pane.Id, out var events)) chat.Replay(events);
                if (item.State == "ended") chat.MarkPaneEnded();
            }
            else
            {
                var scrollback = snapshot.Scrollback.TryGetValue(pane.Id, out var value) ? value : "";
                _scrollback[pane.Id] = scrollback;
                var size = snapshot.Sizes.GetValueOrDefault(pane.Id);
                if (size is not null) _sizes[pane.Id] = size;
                if (resetTerminals)
                {
                    TerminalOutput?.Invoke(this, new TerminalOutputEventArgs(pane.Id, scrollback, true, size));
                }
            }
        }
        Layout = PaneLayout.Reconcile(PaneLayout.Parse(snapshot.Layout), snapshot.Panes.Select(pane => pane.Id));
        LayoutRestored?.Invoke(this, EventArgs.Empty);
        var selectedId = previousSelection is not null && liveIds.Contains(previousSelection)
            ? previousSelection
            : snapshot.ActivePaneId ?? snapshot.Panes.FirstOrDefault()?.Id;
        SelectedPane = Panes.FirstOrDefault(pane => pane.Id == selectedId);
        WorkspaceChanged?.Invoke(this, EventArgs.Empty);
    }

    private void Apply(DaemonEvent item)
    {
        switch (item.Kind)
        {
            case "pty_output":
                var paneId = item.String("pane_id");
                var data = item.String("data");
                if (paneId is not null && data is not null)
                {
                    AppendScrollback(paneId, data);
                    TerminalOutput?.Invoke(this, new TerminalOutputEventArgs(paneId, data, false));
                }
                break;
            case "pane_ended":
                MarkEnded(item.String("pane_id"), item.Integer("exit_code"));
                break;
            case "pane_created":
            case "pane_renamed":
                if (item.Value("pane") is { } paneValue && paneValue.Deserialize<Pane>() is { } pane)
                {
                    Upsert(pane);
                    WorkspaceChanged?.Invoke(this, EventArgs.Empty);
                }
                break;
            case "pane_closed":
                var closedId = item.String("pane_id");
                var closed = Panes.FirstOrDefault(pane => pane.Id == closedId);
                if (closed is not null)
                {
                    Panes.Remove(closed);
                    _chats.Remove(closed.Id);
                    _scrollback.Remove(closed.Id);
                    _sizes.Remove(closed.Id);
                    _outputWarnings.Remove(closed.Id);
                    Layout = Layout?.Remove(closed.Id);
                    if (SelectedPane == closed) SelectedPane = Panes.FirstOrDefault();
                    WorkspaceChanged?.Invoke(this, EventArgs.Empty);
                }
                break;
            case "agent_state":
                var statePane = Panes.FirstOrDefault(pane => pane.Id == item.String("pane_id"));
                if (statePane is not null)
                {
                    statePane.Attention = item.String("attention");
                    statePane.Mode = item.String("mode");
                    statePane.Unattended = item.Payload.TryGetProperty("unattended", out var flag)
                        && flag.ValueKind == JsonValueKind.True;
                }
                break;
            case "projects_changed":
                var projects = ProjectBoard.ParseProjects(item.Payload);
                if (projects is not null)
                {
                    _projects = new Dictionary<string, Project>(projects, StringComparer.Ordinal);
                    foreach (var member in Panes) member.ProjectName = ProjectBoard.ProjectFor(member.Id, _projects);
                    WorkspaceChanged?.Invoke(this, EventArgs.Empty);
                }
                break;
            case "output_warning":
                var warnedId = ProjectBoard.ApplyWarning(item.Payload, _outputWarnings);
                var warned = warnedId is null ? null : Panes.FirstOrDefault(pane => pane.Id == warnedId);
                if (warned is not null)
                {
                    warned.OutputWarning = _outputWarnings.GetValueOrDefault(warnedId!);
                    WorkspaceChanged?.Invoke(this, EventArgs.Empty);
                }
                break;
            case "lease_state":
                var leaseMap = new Dictionary<string, LeaseInfo>(StringComparer.Ordinal);
                var leasePaneId = LeaseState.Apply(item.Payload, leaseMap);
                var leasePane = leasePaneId is null ? null : Panes.FirstOrDefault(pane => pane.Id == leasePaneId);
                if (leasePane is not null)
                {
                    ApplyLease(leasePane, leaseMap.GetValueOrDefault(leasePaneId!));
                    WorkspaceChanged?.Invoke(this, EventArgs.Empty);
                }
                break;
            case "agent_event":
                var agentId = item.String("pane_id");
                if (agentId is not null && item.Value("payload") is { } payload)
                {
                    ChatFor(agentId).Apply(payload);
                }
                break;
            case "config_changed":
                _ = RefreshConfigurationAsync();
                break;
        }
    }

    private PaneViewModel Upsert(Pane pane)
    {
        var existing = Panes.FirstOrDefault(item => item.Id == pane.Id);
        if (existing is not null)
        {
            existing.Title = pane.Title;
            return existing;
        }
        var created = new PaneViewModel(pane);
        Panes.Add(created);
        Layout = PaneLayout.Reconcile(Layout, Panes.Select(item => item.Id));
        return created;
    }

    private void MarkEnded(string? paneId, int? exitCode)
    {
        var pane = Panes.FirstOrDefault(item => item.Id == paneId);
        if (pane is null) return;
        pane.State = "ended";
        if (pane.IsAgent) ChatFor(pane.Id).MarkPaneEnded(exitCode);
        WorkspaceChanged?.Invoke(this, EventArgs.Empty);
    }

    private void AppendScrollback(string paneId, string data)
    {
        var next = (_scrollback.TryGetValue(paneId, out var prior) ? prior : "") + data;
        const int maximum = 4 * 1024 * 1024;
        _scrollback[paneId] = next.Length > maximum ? next[^maximum..] : next;
    }

    private async Task<T?> RunRequestAsync<T>(Func<Task<T>> operation, bool showError = true) where T : class
    {
        var generation = _generation;
        try
        {
            var result = await operation();
            return generation == _generation ? result : null;
        }
        catch (Exception error)
        {
            if (showError && generation == _generation) ErrorMessage = error.Message;
            return null;
        }
    }

    private static Dictionary<string, object?> Request(params (string Key, object? Value)[] values) =>
        values.ToDictionary(value => value.Key, value => value.Value, StringComparer.Ordinal);

    private void Fail(Exception error)
    {
        Status = "Connection failed";
        ErrorMessage = error.Message;
        App.CompleteSmoke(error);
    }

    public void DismissError() => ErrorMessage = null;

    public async Task<JsonObject> ReadConfigurationAsync()
    {
        var client = _client ?? throw new InvalidOperationException("Connect to a workspace first.");
        var generation = _generation;
        var config = await client.RequestAsync<JsonObject>(Request(("command", "get_config")));
        if (generation != _generation) throw new OperationCanceledException("Workspace changed.");
        PermissionMode = config["agent_permission_mode"]?.GetValue<string>() ?? "manual";
        WorkspaceChanged?.Invoke(this, EventArgs.Empty);
        return config;
    }

    private async Task RefreshConfigurationAsync()
    {
        try { await ReadConfigurationAsync(); }
        catch (Exception error) { App.TraceSmoke($"Workspace settings refresh: {error.Message}"); }
    }

    public async Task<WorkspaceSnapshot> ReadSnapshotAsync()
    {
        var client = _client ?? throw new InvalidOperationException("Connect to a workspace first.");
        return await BootstrapAsync(client, _connectionCancellation?.Token ?? CancellationToken.None);
    }

    public async Task WriteConfigurationAsync(JsonObject config, Guid generation)
    {
        if (generation != _generation) throw new OperationCanceledException("Workspace changed. Reopen settings.");
        var client = _client ?? throw new InvalidOperationException("Connect to a workspace first.");
        await client.RequestAsync<CommandOk>(Request(("command", "write_config"), ("config", config)));
        if (generation == _generation) await RefreshConfigurationAsync();
    }

    public void ToggleZoom() { Zoomed = !Zoomed; WorkspaceChanged?.Invoke(this, EventArgs.Empty); }

    public async Task FocusNextAsync(int offset)
    {
        var ids = Layout?.PaneIds.ToList() ?? Panes.Select(pane => pane.Id).ToList();
        if (ids.Count == 0) return;
        var index = Math.Max(0, ids.IndexOf(SelectedPane?.Id ?? ""));
        await SelectAsync(Panes.FirstOrDefault(pane => pane.Id == ids[(index + offset + ids.Count) % ids.Count]));
    }

    public void ResizeSplit(string id, double ratio)
    {
        Layout = Layout?.Resize(id, ratio);
        SaveLayout();
    }

    private async void SaveLayout()
    {
        _layoutSaveCancellation?.Cancel();
        _layoutSaveCancellation?.Dispose();
        _layoutSaveCancellation = new CancellationTokenSource();
        var token = _layoutSaveCancellation.Token;
        var client = _client;
        var generation = _generation;
        var layout = Layout;
        try
        {
            await Task.Delay(200, token);
            if (client is null || generation != _generation) return;
            await client.RequestAsync<CommandOk>(Request(("command", "update_workspace_layout"), ("layout", layout)), token);
        }
        catch (OperationCanceledException) when (token.IsCancellationRequested) { }
        catch (Exception error) { if (generation == _generation) ErrorMessage = error.Message; }
    }

    private void SaveSettings()
    {
        try { new AppSettings(WorkspacePath, TerminalFontSize, RecentWorkspaces).Save(); }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException)
        { ErrorMessage = $"Could not save settings: {error.Message}"; }
    }
}

public sealed class ChatChangedEventArgs(string paneId) : EventArgs
{
    public string PaneId { get; } = paneId;
}

public sealed class TerminalOutputEventArgs(string paneId, string data, bool reset, PaneSize? size = null) : EventArgs
{
    public string PaneId { get; } = paneId;
    public string Data { get; } = data;
    public bool Reset { get; } = reset;
    public PaneSize? Size { get; } = size;
}
