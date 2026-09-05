using System.Text.Json;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.Web.WebView2.Core;
using Sgian.Windows.Terminal;

namespace Sgian.Windows.Views;

public sealed partial class TerminalPaneView : UserControl
{
    private readonly List<string> _pendingMessages = [];
    private CancellationTokenSource? _resizeDebounce;
    private Func<string, Task>? _input;
    private Func<ushort, ushort, Task>? _resize;
    private bool _ready;

    public TerminalPaneView()
    {
        InitializeComponent();
        Unloaded += (_, _) => _resizeDebounce?.Cancel();
    }

    public event EventHandler? Ready;
    public string PaneId { get; private set; } = "";

    public async Task InitializeAsync(
        string paneId,
        string scrollback,
        double fontSize,
        Func<string, Task> input,
        Func<ushort, ushort, Task> resize)
    {
        PaneId = paneId;
        _input = input;
        _resize = resize;
        App.TraceSmoke($"Initializing WebView2 for {paneId}");
        try
        {
            await TerminalWebView.EnsureCoreWebView2Async();
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
            Queue(new { type = "font-size", value = fontSize });
            Queue(new { type = "reset", data = scrollback });
            App.TraceSmoke($"Navigating terminal document for {paneId}");
            TerminalWebView.Source = new Uri(TerminalBridgePolicy.DocumentUrl);
        }
        catch (Exception error)
        {
            App.CompleteSmoke(error);
            throw;
        }
    }

    public void Write(string data) => Queue(new { type = "output", data });
    public void Reset(string data) => Queue(new { type = "reset", data });
    public void SetFontSize(double value) => Queue(new { type = "font-size", value });
    public void FocusTerminal() => Queue(new { type = "focus" });

    private async void WebMessageReceived(CoreWebView2 sender, CoreWebView2WebMessageReceivedEventArgs args)
    {
        if (!TerminalBridgePolicy.IsTrustedDocument(args.Source) ||
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
                await _input(root.GetProperty("data").GetString() ?? "");
            }
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
