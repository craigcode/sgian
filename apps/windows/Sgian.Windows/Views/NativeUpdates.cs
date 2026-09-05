using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Windows.ApplicationModel;
using Windows.System;

namespace Sgian.Windows.Views;

internal static class NativeUpdates
{
    public static async Task CheckAsync(XamlRoot root)
    {
        string message;
        try
        {
            var package = Package.Current;
            var source = package.GetAppInstallerInfo()?.Uri;
            if (source is null)
            {
                message = "Install Sgian using its signed App Installer download to receive automatic updates. Portable development builds do not update automatically.";
            }
            else
            {
                var result = await package.CheckUpdateAvailabilityAsync();
                if (result.Availability is PackageUpdateAvailability.Available or PackageUpdateAvailability.Required)
                {
                    var confirm = new ContentDialog { XamlRoot = root, Title = "A Sgian update is available",
                        Content = "Windows App Installer will verify and install the signed update. Save your work before restarting the app.",
                        PrimaryButtonText = "Open App Installer", CloseButtonText = "Later" };
                    if (await confirm.ShowAsync() == ContentDialogResult.Primary)
                        await Launcher.LaunchUriAsync(new Uri($"ms-appinstaller:?source={Uri.EscapeDataString(source.AbsoluteUri)}"));
                    return;
                }
                message = result.Availability == PackageUpdateAvailability.NoUpdates ? "You’re using the latest version of Sgian."
                    : "Updates could not be checked. Check your connection and try again.";
            }
        }
        catch (InvalidOperationException) { message = "This portable development build does not receive updates. Install a signed Sgian release using App Installer."; }
        catch (Exception error) { message = $"Updates could not be checked: {error.Message}"; }
        await new ContentDialog { XamlRoot = root, Title = "Sgian updates", Content = message, CloseButtonText = "Close" }.ShowAsync();
    }
}
