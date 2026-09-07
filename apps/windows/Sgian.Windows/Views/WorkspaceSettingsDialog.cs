using System.Text.Json.Nodes;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Sgian.Windows.ViewModels;

namespace Sgian.Windows.Views;

internal static class WorkspaceSettingsDialog
{
    public static async Task ShowAsync(XamlRoot root, WorkspaceViewModel model)
    {
        var generation = model.Generation;
        JsonObject config;
        try { config = await model.ReadConfigurationAsync(); }
        catch (Exception error)
        {
            await new ContentDialog { XamlRoot = root, Title = "Could not load settings", Content = error.Message, CloseButtonText = "Close" }.ShowAsync();
            return;
        }
        var panel = new StackPanel { Spacing = 12, MinWidth = 440 };
        var font = new Slider { Header = "Terminal font size", Minimum = 9, Maximum = 30, StepFrequency = 1, Value = model.TerminalFontSize };
        font.ValueChanged += (_, args) => model.TerminalFontSize = args.NewValue;
        panel.Children.Add(font);
        TextBox Field(string title, string key, bool multiline = false)
        {
            var box = new TextBox { Header = title, Text = config[key]?.GetValue<string>() ?? "", AcceptsReturn = multiline };
            panel.Children.Add(box); return box;
        }
        var shell = Field("Shell executable", "shell");
        var shellArgs = new TextBox { Header = "Shell arguments (one per line)", AcceptsReturn = true,
            Text = string.Join("\n", config["shell_args"]?.AsArray().Select(item => item?.GetValue<string>()) ?? []) };
        panel.Children.Add(shellArgs);
        var claude = Field("Claude executable (optional)", "agent_claude_bin");
        var droid = Field("Droid executable (optional)", "agent_droid_bin");
        panel.Children.Add(new TextBlock { Text = "Agents use the provider CLI’s existing installation and login. Changes apply to newly started sessions.", TextWrapping = TextWrapping.Wrap });
        var permissions = new ComboBox { Header = "Agent permission mode", ItemsSource = new[] { "manual", "auto", "dontAsk", "bypassPermissions" }, SelectedItem = config["agent_permission_mode"]?.GetValue<string>() ?? "manual", HorizontalAlignment = HorizontalAlignment.Stretch };
        panel.Children.Add(permissions);
        panel.Children.Add(new TextBlock { Text = "Manual asks for approval. Other modes can run tools without asking.", TextWrapping = TextWrapping.Wrap });
        var restore = new ComboBox { Header = "Session restoration", ItemsSource = new[] { "auto_respawn", "restore_on_demand" }, SelectedItem = config["restore_policy"]?.GetValue<string>() ?? "auto_respawn", HorizontalAlignment = HorizontalAlignment.Stretch };
        panel.Children.Add(restore);
        var profiles = (config["profiles"]?.DeepClone() as JsonArray) ?? new JsonArray();
        var profileList = new StackPanel { Spacing = 6 };
        void RefreshProfiles()
        {
            profileList.Children.Clear();
            foreach (var item in profiles.ToList())
            {
                if (item is null) continue;
                var row = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 10 };
                row.Children.Add(new TextBlock { Text = $"{item["name"]?.GetValue<string>()} · {item["kind"]?.GetValue<string>()}", VerticalAlignment = VerticalAlignment.Center });
                var remove = new Button { Content = "Remove" };
                remove.Click += (_, _) => { profiles.Remove(item); RefreshProfiles(); };
                row.Children.Add(remove); profileList.Children.Add(row);
            }
        }
        panel.Children.Add(new TextBlock { Text = "Profiles", FontWeight = Microsoft.UI.Text.FontWeights.SemiBold });
        panel.Children.Add(profileList); RefreshProfiles();
        var name = new TextBox { Header = "New profile name" };
        var kind = new ComboBox { Header = "Profile type", ItemsSource = new[] { "shell", "claude", "droid" }, SelectedIndex = 0 };
        var profileModel = new TextBox { Header = "Agent model (optional)" };
        panel.Children.Add(name); panel.Children.Add(kind); panel.Children.Add(profileModel);
        var message = new TextBlock { TextWrapping = TextWrapping.Wrap };
        var add = new Button { Content = "Add profile" };
        add.Click += (_, _) =>
        {
            var title = name.Text.Trim();
            if (title.Length == 0 || profiles.Any(item => item?["name"]?.GetValue<string>() == title)) { message.Text = "Enter a unique profile name."; return; }
            var type = kind.SelectedItem as string ?? "shell";
            var profile = new JsonObject { ["name"] = title, ["kind"] = type == "shell" ? "shell" : "agent" };
            if (type != "shell") { profile["backend"] = type; if (!string.IsNullOrWhiteSpace(profileModel.Text)) profile["model"] = profileModel.Text.Trim(); }
            profiles.Add(profile); RefreshProfiles(); name.Text = ""; message.Text = "Save to apply the new profile.";
        };
        panel.Children.Add(add); panel.Children.Add(message);
        var dialog = new ContentDialog { XamlRoot = root, Title = $"Settings · {Path.GetFileName(model.WorkspacePath)}",
            Content = new ScrollViewer { Content = panel, MaxHeight = 520 }, PrimaryButtonText = "Save", CloseButtonText = "Cancel" };
        dialog.PrimaryButtonClick += async (_, args) =>
        {
            var deferral = args.GetDeferral();
            dialog.IsPrimaryButtonEnabled = false;
            try
            {
                var next = (JsonObject)config.DeepClone();
                next["shell"] = string.IsNullOrWhiteSpace(shell.Text) ? null : shell.Text;
                next["shell_args"] = new JsonArray(shellArgs.Text.Split('\n', StringSplitOptions.RemoveEmptyEntries).Select(value => (JsonNode?)JsonValue.Create(value.TrimEnd('\r'))).ToArray());
                next["agent_claude_bin"] = string.IsNullOrWhiteSpace(claude.Text) ? null : claude.Text;
                next["agent_droid_bin"] = string.IsNullOrWhiteSpace(droid.Text) ? null : droid.Text;
                next["agent_permission_mode"] = permissions.SelectedItem as string;
                next["restore_policy"] = restore.SelectedItem as string;
                next["profiles"] = profiles.DeepClone();
                await model.WriteConfigurationAsync(next, generation);
            }
            catch (Exception error) { args.Cancel = true; message.Text = error.Message; }
            finally { dialog.IsPrimaryButtonEnabled = true; deferral.Complete(); }
        };
        await dialog.ShowAsync();
    }
}
