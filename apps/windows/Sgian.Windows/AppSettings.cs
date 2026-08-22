using System.Text.Json;

namespace Sgian.Windows;

internal sealed record AppSettings(string? WorkspacePath, double TerminalFontSize)
{
    private static string SettingsPath => Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
        "Sgian",
        "native-windows.json");

    public static AppSettings Load()
    {
        try
        {
            return JsonSerializer.Deserialize<AppSettings>(File.ReadAllText(SettingsPath))
                ?? new AppSettings(null, 13);
        }
        catch (IOException)
        {
            return new AppSettings(null, 13);
        }
        catch (JsonException)
        {
            return new AppSettings(null, 13);
        }
    }

    public void Save()
    {
        var directory = Path.GetDirectoryName(SettingsPath)!;
        Directory.CreateDirectory(directory);
        var temporary = SettingsPath + ".tmp";
        File.WriteAllText(temporary, JsonSerializer.Serialize(this));
        File.Move(temporary, SettingsPath, true);
    }
}
