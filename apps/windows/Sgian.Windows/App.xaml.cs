using Microsoft.UI.Xaml;

namespace Sgian.Windows;

public partial class App : Application
{
    private Window? _window;

    public App()
    {
        TraceSmoke("App constructor entered");
        UnhandledException += (_, args) =>
        {
            TraceSmoke($"Unhandled exception: {args.Exception}");
            CompleteSmoke(args.Exception);
        };
        InitializeComponent();
        TraceSmoke("Application XAML initialized");
    }

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        TraceSmoke("Creating main window");
        _window = new MainWindow();
        TraceSmoke("Activating main window");
        _window.Activate();
    }

    internal static void TraceSmoke(string message)
    {
        if (Environment.GetEnvironmentVariable("SGIAN_UI_SMOKE") != "1")
        {
            return;
        }
        var marker = Environment.GetEnvironmentVariable("SGIAN_UI_SMOKE_MARKER");
        if (string.IsNullOrWhiteSpace(marker))
        {
            return;
        }
        try
        {
            File.AppendAllText(marker + ".trace", $"{DateTimeOffset.UtcNow:O} {message}{Environment.NewLine}");
        }
        catch (IOException)
        {
        }
        catch (UnauthorizedAccessException)
        {
        }
    }

    internal static void CompleteSmoke(Exception? error = null)
    {
        if (Environment.GetEnvironmentVariable("SGIAN_UI_SMOKE") != "1")
        {
            return;
        }
        var marker = Environment.GetEnvironmentVariable("SGIAN_UI_SMOKE_MARKER");
        if (string.IsNullOrWhiteSpace(marker))
        {
            return;
        }
        try
        {
            TraceSmoke(error is null ? "Smoke completed" : $"Smoke failed: {error}");
            File.WriteAllText(error is null ? marker : marker + ".err", error?.ToString() ?? "ok");
        }
        finally
        {
            Current.Exit();
        }
    }
}
