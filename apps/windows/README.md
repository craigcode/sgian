# Native Windows client

`Sgian.Windows` is the non-Tauri Windows client. It uses WinUI 3 for the app
shell, workspace picker, panes, agent chat, permission prompts and settings. A
local WebView2 hosts the repository's vendored xterm.js files only for terminal
emulation. The existing Rust executable remains the daemon, ConPTY owner and
agent-process host.

The app discovers its owner-scoped named pipe by running:

```powershell
sgian.exe ctl --workspace C:\path\to\project --json ipc-endpoint
```

It reads the token from the returned owner-private path, uses the newline JSON
v1 compatibility wire, and advertises `subscribe-ack`. No token is returned by
the discovery command or passed on a process command line.

On Windows with the .NET 8 SDK and Rust installed:

```powershell
scripts\verify-windows-native.ps1
scripts\build-windows-native.ps1
scripts\smoke-windows-native.ps1 `
  -Exe apps\windows\build\win-x64\Sgian.Windows.exe `
  -Workspace $env:TEMP\sgian-native-smoke `
  -Marker $env:TEMP\sgian-native-smoke-ok
```

The build produces a self-contained portable directory/ZIP for automated smoke
testing and an unsigned MSIX under `apps/windows/build/msix`. The helper is
bundled as `Helpers\sgian.exe`; setting `SGIAN_BACKEND_BINARY` overrides it for
development.
