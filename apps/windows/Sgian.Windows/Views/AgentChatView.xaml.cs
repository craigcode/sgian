using System.Text.Json;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Sgian.Protocol;
using Sgian.Windows.ViewModels;

namespace Sgian.Windows.Views;

public sealed partial class AgentChatView : UserControl
{
    private WorkspaceViewModel? _workspace;
    private PaneViewModel? _pane;

    public AgentChatView()
    {
        InitializeComponent();
    }

    public void Initialize(WorkspaceViewModel workspace, PaneViewModel pane)
    {
        _workspace = workspace;
        _pane = pane;
        Refresh();
    }

    public void Refresh()
    {
        if (_workspace is null || _pane is null) return;
        var chat = _workspace.ChatFor(_pane.Id);
        Messages.ItemsSource = chat.Messages.ToList();
        SessionStatus.Text = BuildStatus(chat);
        PermissionModeNotice.Text = $"Permission mode: {_workspace.PermissionMode}. Tools may run without asking.";
        PermissionModeNotice.Visibility = _workspace.PermissionMode == "manual" ? Visibility.Collapsed : Visibility.Visible;
        PermissionCard.Visibility = chat.PendingPermission is null ? Visibility.Collapsed : Visibility.Visible;
        if (chat.PendingPermission is { } permission)
        {
            PermissionTitle.Text = $"{permission.ToolName} is requesting permission";
            PermissionDetail.Text = permission.Input is { } input
                ? JsonSerializer.Serialize(input, new JsonSerializerOptions { WriteIndented = true })
                : "No additional details.";
        }
        StopButton.Visibility = chat.Busy ? Visibility.Visible : Visibility.Collapsed;
        SendButton.IsEnabled = !chat.Busy && chat.PendingPermission is null &&
            !string.IsNullOrWhiteSpace(Composer.Text);
        if (Messages.Items.Count > 0)
        {
            Messages.ScrollIntoView(Messages.Items[Messages.Items.Count - 1]);
        }
    }

    private static string BuildStatus(AgentChatState chat)
    {
        if (chat.PendingPermission is not null) return "Waiting for permission";
        if (chat.Busy) return "Agent is working…";
        if (chat.Exited) return chat.ExitCode is null ? "Agent stopped" : $"Agent exited ({chat.ExitCode})";
        var identity = string.Join(" · ", new[] { chat.Model, chat.LastTurn?.Label }
            .Where(value => !string.IsNullOrWhiteSpace(value)));
        return identity.Length == 0 ? "Ready" : identity;
    }

    private void Composer_TextChanged(object sender, TextChangedEventArgs e) => Refresh();

    private async void Send_Click(object sender, RoutedEventArgs e)
    {
        if (_workspace is null || _pane is null) return;
        var text = Composer.Text;
        if (string.IsNullOrWhiteSpace(text)) return;
        Composer.Text = "";
        await _workspace.SendAgentMessageAsync(_pane, text);
        Refresh();
    }

    private async void Stop_Click(object sender, RoutedEventArgs e)
    {
        if (_workspace is not null && _pane is not null)
        {
            await _workspace.InterruptAgentAsync(_pane);
        }
    }

    private async void AllowPermission_Click(object sender, RoutedEventArgs e)
    {
        if (_workspace is not null && _pane is not null)
        {
            await _workspace.ResolvePermissionAsync(_pane, true);
        }
    }

    private async void DenyPermission_Click(object sender, RoutedEventArgs e)
    {
        if (_workspace is not null && _pane is not null)
        {
            await _workspace.ResolvePermissionAsync(_pane, false, "Denied in the Sgian Windows client.");
        }
    }
}
