# Windows acceptance handoff

Use the dated handoff ZIP prepared with the release validation report. It
contains the native x64 portable ZIP, the unsigned MSIX from the same CI run,
`smoke-windows-native.ps1`, provenance, and `SHA256SUMS.txt`. The portable build
is for acceptance; the unsigned MSIX cannot satisfy installation, publisher,
SmartScreen or App Installer update acceptance. Leave those gates open until
production-signed candidates exist.

## Start on the Windows machine

1. Copy the handoff ZIP to the Windows machine and extract it into a new folder.
   Read its `BUILD-INFO.json` for the source commit and CI run. Compare the
   outer ZIP's SHA-256 with the adjacent checksum file before extracting.
2. In PowerShell, change into the extracted handoff directory and verify its
   contents. Stop if any checksum differs:

   ```powershell
   Get-Content .\SHA256SUMS.txt | ForEach-Object {
       $expected, $relative = $_ -split '  ', 2
       $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $relative).Hash
       if ($actual -ne $expected) { throw "Checksum mismatch: $relative" }
   }
   Expand-Archive .\artifacts\Sgian-native-windows-win-x64.zip .\portable
   ```

3. Run the packaged UI smoke in a fresh temporary workspace. This checks two
   terminal surfaces, split layout persistence, zoom and search, then shuts
   down only that test workspace. Keep its marker, trace and transcript:

   ```powershell
   $acceptanceRoot = Join-Path $env:TEMP ("sgian-acceptance-" + [guid]::NewGuid().ToString('N'))
   New-Item -ItemType Directory $acceptanceRoot | Out-Null
   Start-Transcript -Path (Join-Path $acceptanceRoot 'smoke.log')
   try {
       & .\smoke-windows-native.ps1 `
           -Exe .\portable\Sgian.Windows.exe `
           -Workspace (Join-Path $acceptanceRoot 'smoke') `
           -Marker (Join-Path $acceptanceRoot 'smoke-ok')
   } finally { Stop-Transcript }
   ```

   If Windows blocks the script, inspect the supplied script and use
   `Unblock-File .\smoke-windows-native.ps1` for this downloaded file only.
   Do not change the machine's execution policy or disable security controls.
   If WebView2 is missing, install Microsoft's Evergreen runtime through its
   official installer. Record the Windows build, CPU architecture, WebView2
   version and any startup error in the results.

4. Start the portable app for the hands-on pass with an isolated workspace:

   ```powershell
   $manualWorkspace = Join-Path $acceptanceRoot 'project-a'
   New-Item -ItemType Directory $manualWorkspace | Out-Null
   $env:SGIAN_WORKSPACE = $manualWorkspace
   try { Start-Process .\portable\Sgian.Windows.exe }
   finally { Remove-Item Env:SGIAN_WORKSPACE -ErrorAction SilentlyContinue }
   ```

   Keep the complete `portable` folder together, including `Helpers` and
   `Terminal`. Rust, Node and the .NET SDK are only needed to rebuild from
   source; they are not required to run this self-contained portable package.

## Hands-on checks

Record each result as pass, fail or not run in `RESULTS.md`, with reproduction
steps and screenshots for failures. A successful automated smoke does not
complete these checks.

- [ ] Launch, type into a shell, create horizontal and vertical splits, drag
      their separators, resize the window repeatedly, zoom and unzoom.
- [ ] Run an available, authenticated provider TUI at full size and in small
      panes. Check borders, the first character of every row, Unicode, pasted
      multiline text, copy, search and scrollback. Record provider/version.
      Claude, Codex and Gemini are supported; only exercise accounts already
      available. **Droid/Factory live testing is deferred: no subscription.**
- [ ] Quit and reopen the client while the daemon is running. Confirm panes,
      layout, scrollback and the live provider session survive.
- [ ] Switch between two temporary projects, each with a `term-1`. Print a
      different marker in each, switch back and forth, and confirm content
      and pending input stay within their workspace.
- [ ] Exercise keyboard take/release, lease refusal, unattended indicators,
      permission allow/deny, project views, saved machines and connection
      recovery where an authorized test endpoint is available.
- [ ] While attached, run the 32-client control-plane burst below. All calls
      must exit successfully; no error 231, missing panes or hung UI.
- [ ] Suspend/resume the machine with the app open. Check reconnect and input.
- [ ] Use Narrator to navigate panes, terminal output, search and splitters.

```powershell
$helper = (Resolve-Path .\portable\Helpers\sgian.exe).Path
$jobs = 1..32 | ForEach-Object {
    Start-Job -ArgumentList $helper,$manualWorkspace -ScriptBlock {
        param($exe, $workspace)
        & $exe ctl --workspace $workspace panes --json
        if ($LASTEXITCODE -ne 0) { throw "ctl failed: $LASTEXITCODE" }
    }
}
try {
    $jobs | Wait-Job -Timeout 120 | Out-Null
    $jobs | Receive-Job
    if (@($jobs | Where-Object State -ne 'Completed').Count) {
        throw 'At least one control-plane request failed or timed out'
    }
} finally {
    $jobs | Stop-Job
    $jobs | Remove-Job
}
```

If the TUI staircases or loses its first column, save a screenshot and capture
the pane's output with `.\portable\Helpers\sgian.exe ctl --workspace <folder> attach <pane>`
while reproducing (stop the capture with Control-C),
and compare the same provider/version in VS Code's terminal at the same size.
Inspect captured output before sharing it; terminal contents can include
private project data. Include the candidate SHA and the smallest reproducer.

When done, quit the acceptance client and shut down each temporary workspace
you created using `.\portable\Helpers\sgian.exe ctl --workspace <folder> shutdown`.
Retain the results and evidence until reviewed. The signed MSIX install,
publisher identity, clean-machine trust and installed-version update tests
remain separate release gates in [native-release.md](native-release.md).
