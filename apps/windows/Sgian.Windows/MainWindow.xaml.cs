using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Sgian.Protocol;
using Windows.UI;
using Windows.System;
using Microsoft.UI.Xaml.Input;
using Sgian.Windows.ViewModels;
using Sgian.Windows.Views;
using Windows.Graphics;
using Windows.Storage.Pickers;

namespace Sgian.Windows;

public sealed partial class MainWindow : Window
{
    private readonly Dictionary<string, TerminalPaneView> _terminals = [];
    private readonly Dictionary<string, AgentChatView> _agentChats = [];
    private bool _synchronizingSelection;
    private bool _started;
    private readonly Dictionary<string, Border> _paneFrames = [];
    private string? _renderedShape;
    private bool _nativeSmokeRunning;
    private string? _focusedPaneId;

    public MainWindow()
    {
        InitializeComponent();
        ViewModel = new WorkspaceViewModel(DispatcherQueue);
        PaneList.ItemsSource = ViewModel.Panes;
        ErrorBar.Closed += (_, _) => ViewModel.DismissError();
        ViewModel.WorkspaceReset += (_, _) => ClearViews();
        ViewModel.LayoutRestored += (_, _) => _renderedShape = null;
        ViewModel.WorkspaceChanged += (_, _) => Refresh();
        ViewModel.ChatChanged += (_, args) =>
        {
            if (_agentChats.TryGetValue(args.PaneId, out var view)) view.Refresh();
        };
        ViewModel.TerminalOutput += (_, args) =>
        {
            if (!_terminals.TryGetValue(args.PaneId, out var view)) return;
            if (args.Reset) view.Reset(args.Data, args.Size); else view.Write(args.Data);
        };
        ViewModel.TerminalSettingsChanged += (_, _) =>
            _terminals.Values.ToList().ForEach(view => view.SetFontSize(ViewModel.TerminalFontSize));
        ViewModel.PropertyChanged += (_, args) =>
        {
            if (args.PropertyName is nameof(ViewModel.Status) or nameof(ViewModel.ErrorMessage) or
                nameof(ViewModel.WorkspacePath)) RefreshChrome();
        };
        Closed += MainWindow_Closed;
        AddShortcut(VirtualKey.D, VirtualKeyModifiers.Control | VirtualKeyModifiers.Shift, () => ViewModel.CreateShellAsync("row"));
        AddShortcut(VirtualKey.E, VirtualKeyModifiers.Control | VirtualKeyModifiers.Shift, () => ViewModel.CreateShellAsync("column"));
        AddShortcut(VirtualKey.Z, VirtualKeyModifiers.Control | VirtualKeyModifiers.Shift, () => { ViewModel.ToggleZoom(); return Task.CompletedTask; });
        AddShortcut(VirtualKey.F, VirtualKeyModifiers.Control | VirtualKeyModifiers.Shift, ShowSearchAsync);
        AddShortcut(VirtualKey.P, VirtualKeyModifiers.Control | VirtualKeyModifiers.Shift, ShowCommandsAsync);
        AddShortcut(VirtualKey.Tab, VirtualKeyModifiers.Control, () => ViewModel.FocusNextAsync(1));
        AddShortcut(VirtualKey.Tab, VirtualKeyModifiers.Control | VirtualKeyModifiers.Shift, () => ViewModel.FocusNextAsync(-1));

        var appWindow = GetAppWindow();
        appWindow.Resize(new SizeInt32(1180, 760));
        Activated += MainWindow_Activated;
    }

    public WorkspaceViewModel ViewModel { get; }

    private async void MainWindow_Activated(object sender, WindowActivatedEventArgs args)
    {
        if (_started) return;
        _started = true;
        App.TraceSmoke("Main window activated; connecting workspace");
        await ViewModel.StartAsync();
        App.TraceSmoke("Initial workspace connection completed");
    }

    private AppWindow GetAppWindow()
    {
        var windowHandle = WinRT.Interop.WindowNative.GetWindowHandle(this);
        var windowId = Microsoft.UI.Win32Interop.GetWindowIdFromWindow(windowHandle);
        return AppWindow.GetFromWindowId(windowId);
    }

    private void Refresh()
    {
        RefreshChrome();
        _synchronizingSelection = true;
        PaneList.SelectedItem = ViewModel.SelectedPane;
        _synchronizingSelection = false;
        var pane = ViewModel.SelectedPane;
        PaneTitle.Text = pane?.Title ?? "Sgian";
        PaneSubtitle.Text = pane?.Subtitle ?? "Native terminals and coding agents";
        RenameButton.Visibility = pane is null ? Visibility.Collapsed : Visibility.Visible;
        CloseButton.Visibility = pane is null ? Visibility.Collapsed : Visibility.Visible;
        RestartButton.Visibility = pane?.State == "ended"
            ? Visibility.Visible
            : Visibility.Collapsed;
        ShowLayout();
    }

    private void RefreshChrome()
    {
        ConnectionStatus.Text = ViewModel.Status;
        WorkspaceLabel.Text = Path.GetFileName(ViewModel.WorkspacePath.TrimEnd(Path.DirectorySeparatorChar));
        ToolTipService.SetToolTip(WorkspaceLabel, ViewModel.WorkspacePath);
        ErrorBar.Message = ViewModel.ErrorMessage ?? "";
        ErrorBar.IsOpen = !string.IsNullOrWhiteSpace(ViewModel.ErrorMessage);
    }

    private void ClearViews()
    {
        PaneContent.Content = null;
        foreach (var terminal in _terminals.Values) terminal.Dispose();
        _terminals.Clear();
        _agentChats.Clear();
        _paneFrames.Clear();
        _renderedShape = null;
        _focusedPaneId = null;
    }

    private void ShowLayout()
    {
        var live = ViewModel.Panes.Select(pane => pane.Id).ToHashSet();
        foreach (var id in _paneFrames.Keys.Where(id => !live.Contains(id)).ToList())
        {
            if (_terminals.Remove(id, out var terminal)) terminal.Dispose();
            _agentChats.Remove(id);
            _paneFrames.Remove(id);
        }
        var tree = ViewModel.Zoomed && ViewModel.SelectedPane is { } selected
            ? PaneLayout.Leaf(selected.Id) : ViewModel.Layout;
        if (tree is null) { PaneContent.Content = EmptyWorkspaceContent(); _renderedShape = null; return; }
        string Shape(PaneLayout node) => node.IsLeaf ? node.Id : $"{node.Id}:{node.Direction}({Shape(node.First!)},{Shape(node.Second!)})";
        var shape = Shape(tree);
        if (_renderedShape != shape)
        {
            PaneContent.Content = null;
            foreach (var frame in _paneFrames.Values)
                if (frame.Parent is Panel panel) panel.Children.Remove(frame);
                else if (frame.Parent is ContentControl content) content.Content = null;
            PaneContent.Content = NativeLayoutView.Build(tree, PaneFrame, ViewModel.ResizeSplit);
            _renderedShape = shape;
        }
        foreach (var (id, frame) in _paneFrames)
        {
            frame.BorderBrush = new SolidColorBrush(id == ViewModel.SelectedPane?.Id
                ? Microsoft.UI.Colors.DodgerBlue : Microsoft.UI.Colors.Transparent);
            if (_agentChats.TryGetValue(id, out var chat)) chat.Refresh();
        }
        if (ViewModel.SelectedPane is { IsAgent: false } pane && _focusedPaneId != pane.Id && _terminals.TryGetValue(pane.Id, out var active))
        { _focusedPaneId = pane.Id; active.FocusTerminal(); }
    }

    private FrameworkElement PaneFrame(string id)
    {
        if (_paneFrames.TryGetValue(id, out var existing)) return existing;
        var pane = ViewModel.Panes.First(item => item.Id == id);
        var generation = ViewModel.Generation;
        var grid = new Grid();
        grid.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        grid.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
        var header = new Button { HorizontalAlignment = HorizontalAlignment.Stretch, HorizontalContentAlignment = HorizontalAlignment.Left, Padding = new Thickness(10, 5, 10, 5) };
        header.SetBinding(ContentControl.ContentProperty, new Microsoft.UI.Xaml.Data.Binding { Source = pane, Path = new PropertyPath(nameof(pane.Title)), Mode = Microsoft.UI.Xaml.Data.BindingMode.OneWay });
        header.Click += async (_, _) => { if (ViewModel.Generation == generation) await ViewModel.SelectAsync(pane); };
        grid.Children.Add(header);
        FrameworkElement view;
        if (pane.IsAgent)
        {
            var chat = new AgentChatView();
            chat.Initialize(ViewModel, pane);
            _agentChats[id] = chat;
            view = chat;
        }
        else
        {
            var terminal = new TerminalPaneView();
            terminal.Ready += OnTerminalReady;
            terminal.Activated += async (_, _) => { if (ViewModel.Generation == generation && ViewModel.SelectedPane != pane) await ViewModel.SelectAsync(pane); };
            _terminals[id] = terminal;
            view = terminal;
            _ = InitializeTerminalAsync(terminal, pane, generation);
        }
        Grid.SetRow(view, 1);
        grid.Children.Add(view);
        var frame = new Border { BorderThickness = new Thickness(2), Child = grid, MinWidth = 0, MinHeight = 0 };
        _paneFrames[id] = frame;
        return frame;
    }

    private async Task InitializeTerminalAsync(TerminalPaneView terminal, PaneViewModel pane, Guid generation)
    {
        await terminal.InitializeAsync(pane.Id, ViewModel.InitialScrollback(pane.Id), ViewModel.InitialSize(pane.Id), ViewModel.TerminalFontSize,
            data => generation == ViewModel.Generation ? ViewModel.WriteTerminalAsync(pane.Id, data) : Task.CompletedTask,
            (columns, rows) => generation == ViewModel.Generation ? ViewModel.ResizeTerminalAsync(pane.Id, columns, rows) : Task.CompletedTask);
        if (generation == ViewModel.Generation) await ViewModel.EnsureTerminalAsync(pane.Id);
    }

    private async void OnTerminalReady(object? sender, EventArgs args)
    {
        if (Environment.GetEnvironmentVariable("SGIAN_UI_SMOKE") != "1" || _nativeSmokeRunning) return;
        _nativeSmokeRunning = true;
        try
        {
            await ViewModel.CreateShellAsync("column");
            for (var attempt = 0; attempt < 80; attempt++)
            {
                if (_terminals.Count >= 2 && _terminals.Values.All(view => view.IsReady)) break;
                await Task.Delay(100);
            }
            if (_terminals.Count < 2 || _terminals.Values.Any(view => !view.IsReady)) throw new InvalidOperationException("Native split terminals did not initialize");
            await Task.Delay(350);
            var snapshot = await ViewModel.ReadSnapshotAsync();
            if (PaneLayout.Parse(snapshot.Layout)?.PaneIds.Count != 2) throw new InvalidOperationException("Native split layout did not persist");
            var terminal = _terminals.Values.Last();
            var searched = new TaskCompletionSource<bool>(TaskCreationOptions.RunContinuationsAsynchronously);
            terminal.SearchCompleted += result => searched.TrySetResult(result != "No matches");
            terminal.Write("\r\nnative-search-smoke\r\n");
            await Task.Delay(250);
            terminal.Search("native-search-smoke", false);
            if (!await searched.Task.WaitAsync(TimeSpan.FromSeconds(5))) throw new InvalidOperationException("Native terminal search failed");
            App.CompleteSmoke();
        }
        catch (Exception error) { App.CompleteSmoke(error); }
    }

    private void AddShortcut(VirtualKey key, VirtualKeyModifiers modifiers, Func<Task> action)
    {
        var shortcut = new KeyboardAccelerator { Key = key, Modifiers = modifiers };
        shortcut.Invoked += async (_, args) => { args.Handled = true; await action(); };
        Root.KeyboardAccelerators.Add(shortcut);
    }

    private async Task ShowSearchAsync()
    {
        if (ViewModel.SelectedPane is not { } pane || !_terminals.TryGetValue(pane.Id, out var terminal)) return;
        var input = new TextBox { PlaceholderText = "Find in terminal" };
        var status = new TextBlock { Text = "Enter text to search the terminal buffer." };
        var content = new StackPanel { Spacing = 10 }; content.Children.Add(input); content.Children.Add(status);
        void Result(string result) => status.Text = result;
        terminal.SearchCompleted += Result;
        var dialog = new ContentDialog { XamlRoot = Root.XamlRoot, Title = "Find in terminal", Content = content,
            PrimaryButtonText = "Next", SecondaryButtonText = "Previous", CloseButtonText = "Done" };
        dialog.PrimaryButtonClick += (_, args) => { args.Cancel = true; terminal.Search(input.Text, false); };
        dialog.SecondaryButtonClick += (_, args) => { args.Cancel = true; terminal.Search(input.Text, true); };
        try { await dialog.ShowAsync(); }
        finally { terminal.SearchCompleted -= Result; }
        terminal.FocusTerminal();
    }

    private async Task ShowCommandsAsync()
    {
        var actions = new List<(string Label, Func<Task> Run)> {
            ("Split right", () => ViewModel.CreateShellAsync("row")),
            ("Split down", () => ViewModel.CreateShellAsync("column")),
            ("New Claude agent", () => ViewModel.CreateAgentAsync("claude")),
            ("New Factory Droid agent", () => ViewModel.CreateAgentAsync("droid")),
            ("Zoom / Show all panes", () => { ViewModel.ToggleZoom(); return Task.CompletedTask; }),
            ("Reconnect", () => ViewModel.StartAsync()),
            ("Check for updates", () => NativeUpdates.CheckAsync(Root.XamlRoot)),
        };
        actions.AddRange(ViewModel.Panes.Select(pane => ($"Focus: {pane.Title} [{pane.Id}]", (Func<Task>)(() => ViewModel.SelectAsync(pane)))));
        actions.AddRange(ViewModel.RecentWorkspaces.Select(path => ($"Workspace: {path}", (Func<Task>)(() => ViewModel.ConnectAsync(path)))));
        try
        {
            var config = await ViewModel.ReadConfigurationAsync();
            if (config["profiles"] is System.Text.Json.Nodes.JsonArray profiles)
                foreach (var profile in profiles)
                    if (profile?["name"]?.GetValue<string>() is { } name)
                        actions.Add(($"New pane with profile: {name}", () => ViewModel.CreateShellAsync(profile: name)));
        }
        catch (Exception error) { App.TraceSmoke($"Command profile loading: {error.Message}"); }
        var input = new TextBox { PlaceholderText = "Search commands and panes" };
        var list = new ListView { Height = 260, ItemsSource = actions.Select(item => item.Label).ToList() };
        input.TextChanged += (_, _) => list.ItemsSource = actions.Where(item => item.Label.Contains(input.Text, StringComparison.OrdinalIgnoreCase)).Select(item => item.Label).ToList();
        var panel = new StackPanel { Spacing = 12, MinWidth = 420 }; panel.Children.Add(input); panel.Children.Add(list);
        var dialog = new ContentDialog { XamlRoot = Root.XamlRoot, Title = "Commands", Content = panel,
            PrimaryButtonText = "Run", CloseButtonText = "Cancel", DefaultButton = ContentDialogButton.Primary };
        if (await dialog.ShowAsync() == ContentDialogResult.Primary && list.SelectedItem is string label)
            await actions.First(item => item.Label == label).Run();
    }

    private async void SplitRight_Click(object sender, RoutedEventArgs e) => await ViewModel.CreateShellAsync("row");
    private async void SplitDown_Click(object sender, RoutedEventArgs e) => await ViewModel.CreateShellAsync("column");
    private void Zoom_Click(object sender, RoutedEventArgs e) => ViewModel.ToggleZoom();
    private async void Commands_Click(object sender, RoutedEventArgs e) => await ShowCommandsAsync();
    private async void Search_Click(object sender, RoutedEventArgs e) => await ShowSearchAsync();

    private UIElement EmptyWorkspaceContent()
    {
        var panel = new StackPanel
        {
            Spacing = 12,
            HorizontalAlignment = HorizontalAlignment.Center,
            VerticalAlignment = VerticalAlignment.Center,
            MaxWidth = 460,
        };
        panel.Children.Add(new TextBlock
        {
            Text = ViewModel.Status == "Connected" ? "Your workspace is ready" : ViewModel.Status,
            FontSize = 24,
            FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
            HorizontalAlignment = HorizontalAlignment.Center,
        });
        panel.Children.Add(new TextBlock
        {
            Text = ViewModel.ErrorMessage ?? "Create a terminal or agent conversation to get started.",
            TextWrapping = TextWrapping.Wrap,
            TextAlignment = TextAlignment.Center,
            HorizontalAlignment = HorizontalAlignment.Center,
        });
        return panel;
    }

    private async void PaneList_SelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (!_synchronizingSelection)
        {
            await ViewModel.SelectAsync(PaneList.SelectedItem as PaneViewModel);
        }
    }

    private async void NewTerminal_Click(object sender, RoutedEventArgs e) =>
        await ViewModel.CreateShellAsync();

    private async void NewAgent_Click(object sender, RoutedEventArgs e)
    {
        if (sender is MenuFlyoutItem item && item.Tag is string backend)
        {
            await ViewModel.CreateAgentAsync(backend);
        }
    }

    private async void Restart_Click(object sender, RoutedEventArgs e)
    {
        if (ViewModel.SelectedPane is { } pane) await ViewModel.RestartAsync(pane);
    }

    private async void Close_Click(object sender, RoutedEventArgs e)
    {
        if (ViewModel.SelectedPane is not { } pane) return;
        var dialog = new ContentDialog
        {
            XamlRoot = Root.XamlRoot,
            Title = $"Close {pane.Title}?",
            Content = pane.IsAgent
                ? "This closes the agent process and removes the pane from the workspace."
                : "This closes the shell and removes the pane from the workspace.",
            PrimaryButtonText = "Close",
            CloseButtonText = "Cancel",
            DefaultButton = ContentDialogButton.Close,
        };
        if (await dialog.ShowAsync() == ContentDialogResult.Primary)
        {
            await ViewModel.CloseAsync(pane);
        }
    }

    private async void Rename_Click(object sender, RoutedEventArgs e)
    {
        if (ViewModel.SelectedPane is not { } pane) return;
        var input = new TextBox { Text = pane.Title, SelectionStart = pane.Title.Length };
        var dialog = new ContentDialog
        {
            XamlRoot = Root.XamlRoot,
            Title = "Rename pane",
            Content = input,
            PrimaryButtonText = "Rename",
            CloseButtonText = "Cancel",
            DefaultButton = ContentDialogButton.Primary,
        };
        if (await dialog.ShowAsync() == ContentDialogResult.Primary)
        {
            await ViewModel.RenameAsync(pane, input.Text);
        }
    }

    private async void ChooseWorkspace_Click(object sender, RoutedEventArgs e)
    {
        var picker = new FolderPicker
        {
            SuggestedStartLocation = PickerLocationId.ComputerFolder,
            CommitButtonText = "Open workspace",
        };
        picker.FileTypeFilter.Add("*");
        WinRT.Interop.InitializeWithWindow.Initialize(
            picker,
            WinRT.Interop.WindowNative.GetWindowHandle(this));
        var folder = await picker.PickSingleFolderAsync();
        if (folder is not null) await ViewModel.ConnectAsync(folder.Path);
    }

    private async void Settings_Click(object sender, RoutedEventArgs e) =>
        await WorkspaceSettingsDialog.ShowAsync(Root.XamlRoot, ViewModel);

    private async void MainWindow_Closed(object sender, WindowEventArgs args)
    {
        ClearViews();
        await ViewModel.DisposeAsync();
    }
}
