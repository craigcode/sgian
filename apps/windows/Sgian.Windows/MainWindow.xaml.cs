using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
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

    public MainWindow()
    {
        InitializeComponent();
        ViewModel = new WorkspaceViewModel(DispatcherQueue);
        PaneList.ItemsSource = ViewModel.Panes;
        ViewModel.WorkspaceChanged += (_, _) => Refresh();
        ViewModel.ChatChanged += (_, args) =>
        {
            if (_agentChats.TryGetValue(args.PaneId, out var view)) view.Refresh();
        };
        ViewModel.TerminalOutput += (_, args) =>
        {
            if (!_terminals.TryGetValue(args.PaneId, out var view)) return;
            if (args.Reset) view.Reset(args.Data); else view.Write(args.Data);
        };
        ViewModel.TerminalSettingsChanged += (_, _) =>
            _terminals.Values.ToList().ForEach(view => view.SetFontSize(ViewModel.TerminalFontSize));
        ViewModel.PropertyChanged += (_, args) =>
        {
            if (args.PropertyName is nameof(ViewModel.Status) or nameof(ViewModel.ErrorMessage) or
                nameof(ViewModel.WorkspacePath)) RefreshChrome();
        };
        Closed += MainWindow_Closed;

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
        RestartButton.Visibility = pane?.State == "ended" && pane.IsAgent == false
            ? Visibility.Visible
            : Visibility.Collapsed;
        ShowPane(pane);
    }

    private void RefreshChrome()
    {
        ConnectionStatus.Text = ViewModel.Status;
        WorkspaceLabel.Text = Path.GetFileName(ViewModel.WorkspacePath.TrimEnd(Path.DirectorySeparatorChar));
        ToolTipService.SetToolTip(WorkspaceLabel, ViewModel.WorkspacePath);
        ErrorBar.Message = ViewModel.ErrorMessage ?? "";
        ErrorBar.IsOpen = !string.IsNullOrWhiteSpace(ViewModel.ErrorMessage);
    }

    private void ShowPane(PaneViewModel? pane)
    {
        if (pane is null)
        {
            PaneContent.Content = EmptyWorkspaceContent();
            return;
        }
        if (pane.IsAgent)
        {
            if (!_agentChats.TryGetValue(pane.Id, out var chat))
            {
                chat = new AgentChatView();
                chat.Initialize(ViewModel, pane);
                _agentChats[pane.Id] = chat;
            }
            chat.Refresh();
            PaneContent.Content = chat;
            App.CompleteSmoke();
            return;
        }
        if (!_terminals.TryGetValue(pane.Id, out var terminal))
        {
            App.TraceSmoke($"Creating terminal view for {pane.Id}");
            terminal = new TerminalPaneView();
            terminal.Ready += (_, _) => App.CompleteSmoke();
            _terminals[pane.Id] = terminal;
            _ = terminal.InitializeAsync(
                pane.Id,
                ViewModel.InitialScrollback(pane.Id),
                ViewModel.TerminalFontSize,
                data => ViewModel.WriteTerminalAsync(pane.Id, data),
                (columns, rows) => ViewModel.ResizeTerminalAsync(pane.Id, columns, rows));
            _ = ViewModel.EnsureTerminalAsync(pane.Id);
        }
        PaneContent.Content = terminal;
        terminal.FocusTerminal();
    }

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
            _terminals.Remove(pane.Id);
            _agentChats.Remove(pane.Id);
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

    private async void Settings_Click(object sender, RoutedEventArgs e)
    {
        var slider = new Slider
        {
            Minimum = 9,
            Maximum = 30,
            StepFrequency = 1,
            Value = ViewModel.TerminalFontSize,
            Header = "Terminal font size",
        };
        var workspace = new TextBlock
        {
            Text = ViewModel.WorkspacePath,
            TextWrapping = TextWrapping.Wrap,
            Foreground = (Microsoft.UI.Xaml.Media.Brush)Application.Current.Resources["TextFillColorSecondaryBrush"],
        };
        var panel = new StackPanel { Spacing = 12, MinWidth = 420 };
        panel.Children.Add(slider);
        panel.Children.Add(new TextBlock { Text = "Workspace", FontWeight = Microsoft.UI.Text.FontWeights.SemiBold });
        panel.Children.Add(workspace);
        var dialog = new ContentDialog
        {
            XamlRoot = Root.XamlRoot,
            Title = "Sgian settings",
            Content = panel,
            PrimaryButtonText = "Done",
            DefaultButton = ContentDialogButton.Primary,
        };
        slider.ValueChanged += (_, args) => ViewModel.TerminalFontSize = args.NewValue;
        await dialog.ShowAsync();
    }

    private async void MainWindow_Closed(object sender, WindowEventArgs args) =>
        await ViewModel.DisposeAsync();
}
