using System.Text.Json;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.Web.WebView2.Core;
using Sgian.Windows.Terminal;
using Sgian.Protocol;

namespace Sgian.Windows.Views;

public sealed partial class TerminalPaneView : UserControl, IDisposable
{
    private readonly List<string> _pendingMessages = [];
    private CancellationTokenSource? _resizeDebounce;
    private Func<string, Task>? _input;
    private Func<ushort, ushort, Task>? _resize;
    private bool _ready;
    private bool _disposed;
    private readonly SemaphoreSlim _inputGate = new(1, 1);

    public TerminalPaneView()
    {
        InitializeComponent();
        Unloaded += (_, _) => { if (!_disposed) _resizeDebounce?.Cancel(); };
    }

    public event EventHandler? Ready;
    public event EventHandler? Activated;
    public event Action<string>? SearchCompleted;
    public string PaneId { get; private set; } = "";
    public bool IsReady => _ready && !_disposed;

    public async Task InitializeAsync(
        string paneId,
        string scrollback,
        PaneSize? size,
        double fontSize,
        Func<string, Task> input,
        Func<ushort, ushort, Task> resize)
    {
        PaneId = paneId;
        _input = input;
        _resize = resize;
        Queue(new { type = "font-size", value = fontSize });
        Reset(scrollback, size);
        App.TraceSmoke($"Initializing WebView2 for {paneId}");
        try
        {
            await TerminalWebView.EnsureCoreWebView2Async();
            if (_disposed) return;
            App.TraceSmoke($"WebView2 environment ready for {paneId}");
            var terminalDirectory = Path.Combine(AppContext.BaseDirectory, "Terminal");
            TerminalWebView.CoreWebView2.SetVirtualHostNameToFolderMapping(
                "sgian.local",
                terminalDirectory,
                CoreWebView2HostResourceAccessKind.DenyCors);
            TerminalWebView.CoreWebView2.Settings.AreDefaultContextMenusEnabled = false;
            TerminalWebView.CoreWebView2.Settings.AreDevToolsEnabled = false;
            TerminalWebView.CoreWebView2.Settings.IsStatusBarEnabled = false;
            // Only the bundled terminal may access the shell-input bridge.
            // Block drag/drop navigation, links, redirects, frames, and popups.
            TerminalWebView.CoreWebView2.NavigationStarting += (_, args) =>
                args.Cancel = !TerminalBridgePolicy.IsTrustedDocument(args.Uri);
            TerminalWebView.CoreWebView2.FrameNavigationStarting += (_, args) => args.Cancel = true;
            TerminalWebView.CoreWebView2.NewWindowRequested += (_, args) => args.Handled = true;
            TerminalWebView.CoreWebView2.PermissionRequested += (_, args) =>
                args.State = CoreWebView2PermissionState.Deny;
            TerminalWebView.CoreWebView2.DownloadStarting += (_, args) => args.Cancel = true;
            TerminalWebView.CoreWebView2.WebMessageReceived += WebMessageReceived;
            TerminalWebView.NavigationCompleted += (_, args) =>
                App.TraceSmoke($"Terminal navigation for {paneId}: success={args.IsSuccess}, status={args.WebErrorStatus}");
            TerminalWebView.CoreWebView2.ProcessFailed += (_, args) =>
                App.TraceSmoke($"WebView2 process failed for {paneId}: {args.ProcessFailedKind}");
            App.TraceSmoke($"Navigating terminal document for {paneId}");
            TerminalWebView.Source = new Uri(TerminalBridgePolicy.DocumentUrl);
        }
        catch (Exception error)
        {
            if (_disposed) return;
            App.CompleteSmoke(error);
            LoadingIndicator.IsActive = false;
            Content = new TextBlock { Text = $"Terminal could not start: {error.Message}", TextWrapping = TextWrapping.Wrap, Margin = new Thickness(16) };
        }
    }

    public void Write(string data) => Queue(new { type = "output", data });
    public void Reset(string data, PaneSize? size = null) => Queue(new { type = "reset", data, cols = size?.Columns, rows = size?.Rows });
    public void SetFontSize(double value) => Queue(new { type = "font-size", value });
    public void FocusTerminal() => Queue(new { type = "focus" });
    public void Search(string query, bool previous) => Queue(new { type = "search", query, previous });

    public void Dispose()
    {
        if (_disposed) return;
        _disposed = true;
        _resizeDebounce?.Cancel();
        _resizeDebounce?.Dispose();
        _resizeDebounce = null;
        _input = null;
        _resize = null;
        _pendingMessages.Clear();
        TerminalWebView.Close();
    }

    private async void WebMessageReceived(CoreWebView2 sender, CoreWebView2WebMessageReceivedEventArgs args)
    {
        if (_disposed || !TerminalBridgePolicy.IsTrustedDocument(args.Source) ||
            !TerminalBridgePolicy.IsTrustedDocument(sender.Source)) return;
        try
        {
            using var document = JsonDocument.Parse(args.WebMessageAsJson);
            var root = document.RootElement;
            var type = root.GetProperty("type").GetString();
            if (type == "ready")
            {
                App.TraceSmoke($"Terminal bridge ready for {PaneId}");
                _ready = true;
                LoadingIndicator.IsActive = false;
                LoadingIndicator.Visibility = Visibility.Collapsed;
                foreach (var message in _pendingMessages)
                {
                    TerminalWebView.CoreWebView2.PostWebMessageAsJson(message);
                }
                _pendingMessages.Clear();
                Ready?.Invoke(this, EventArgs.Empty);
            }
            else if (type == "input" && _input is not null)
            {
                var data = root.GetProperty("data").GetString() ?? "";
                await _inputGate.WaitAsync();
                try { if (!_disposed && _input is not null) await _input(data); }
                finally { _inputGate.Release(); }
            }
            else if (type == "activated") Activated?.Invoke(this, EventArgs.Empty);
            else if (type == "error") throw new InvalidOperationException(root.GetProperty("message").GetString());
            else if (type == "search-result") SearchCompleted?.Invoke(root.GetProperty("found").GetBoolean() ? "Match selected in terminal" : "No matches");
            else if (type == "resize" && _resize is not null)
            {
                var columns = (ushort)Math.Clamp(root.GetProperty("cols").GetInt32(), 2, ushort.MaxValue);
                var rows = (ushort)Math.Clamp(root.GetProperty("rows").GetInt32(), 2, ushort.MaxValue);
                _resizeDebounce?.Cancel();
                _resizeDebounce?.Dispose();
                _resizeDebounce = new CancellationTokenSource();
                var token = _resizeDebounce.Token;
                try
                {
                    await Task.Delay(75, token);
                    if (_disposed || token.IsCancellationRequested || _resize is null) return;
                    await _resize(columns, rows);
                }
                catch (OperationCanceledException) when (token.IsCancellationRequested)
                {
                }
            }
        }
        catch (Exception error)
        {
            App.CompleteSmoke(error);
        }
    }

    private void Queue(object message)
    {
        if (_disposed) return;
        var json = JsonSerializer.Serialize(message);
        if (_ready && TerminalWebView.CoreWebView2 is not null &&
            TerminalBridgePolicy.IsTrustedDocument(TerminalWebView.CoreWebView2.Source))
        {
            TerminalWebView.CoreWebView2.PostWebMessageAsJson(json);
        }
        else
        {
            _pendingMessages.Add(json);
        }
    }
}
