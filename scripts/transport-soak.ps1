# ENHANCEMENTS §5: multi-round Windows named-pipe transport soak.
# Usage: transport-soak.ps1 -Exe <sgian.exe> -Workspace <dir>
param(
  [Parameter(Mandatory = $true)][string]$Exe,
  [Parameter(Mandatory = $true)][string]$Workspace,
  [int]$Rounds = 3,
  [int]$Burst = 32
)

$ErrorActionPreference = "Stop"
New-Item -ItemType Directory -Force -Path $Workspace | Out-Null

function Invoke-Sgian([string[]]$CmdArgs, [int]$TimeoutMs = 60000) {
  $p = Start-Process -FilePath $Exe -ArgumentList $CmdArgs -PassThru -NoNewWindow
  if (-not $p.WaitForExit($TimeoutMs)) {
    try { $p.Kill() } catch {}
    throw "timed out after $($TimeoutMs/1000)s: sgian $($CmdArgs -join ' ')"
  }
  return $p.ExitCode
}

function Get-SgianHandleCount {
  $proc = Get-Process -Name "sgian" -ErrorAction SilentlyContinue | Select-Object -First 1
  if (-not $proc) { return 0 }
  return [int]$proc.HandleCount
}

if ((Invoke-Sgian @("ctl", "--workspace", $Workspace, "new", "--name", "soak-main")) -ne 0) {
  throw "ctl new soak-main failed"
}
$baselineHandles = Get-SgianHandleCount
Write-Host "TRANSPORT SOAK baseline handles=$baselineHandles"

for ($round = 1; $round -le $Rounds; $round++) {
  Write-Host "TRANSPORT SOAK round $round/$Rounds"
  $burstProcs = foreach ($i in 1..$Burst) {
    Start-Process -FilePath $Exe -ArgumentList @("ctl", "--workspace", $Workspace, "panes", "--json") -PassThru -NoNewWindow
  }
  foreach ($p in $burstProcs) {
    if (-not $p.WaitForExit(60000)) {
      try { $p.Kill() } catch {}
      throw "concurrent named-pipe client timed out (pid $($p.Id))"
    }
    if ($p.ExitCode -ne 0) {
      throw "concurrent named-pipe client failed with exit $($p.ExitCode) (pid $($p.Id))"
    }
  }

  $name = "soak-r$round"
  if ((Invoke-Sgian @("ctl", "--workspace", $Workspace, "new", "--name", $name)) -ne 0) {
    throw "ctl new $name failed"
  }
  if ((Invoke-Sgian @("ctl", "--workspace", $Workspace, "restart", $name)) -ne 0) {
    throw "ctl restart $name failed"
  }
  if ((Invoke-Sgian @("ctl", "--workspace", $Workspace, "send", $name, "echo soak-ok\n")) -ne 0) {
    throw "ctl send $name failed"
  }

  $log = Get-ChildItem (Join-Path $env:APPDATA "Sgian\workspaces\*\daemon.log") -ErrorAction SilentlyContinue |
    Select-Object -First 1
  if ($log) {
    Add-Content -Path $log.FullName -Value "soak-round-$round" -ErrorAction SilentlyContinue
  }

  if ((Invoke-Sgian @("ctl", "--workspace", $Workspace, "panes", "--json")) -ne 0) {
    throw "ctl panes failed after round $round"
  }
}

$afterHandles = Get-SgianHandleCount
Write-Host "TRANSPORT SOAK after handles=$afterHandles"
if ($afterHandles -gt ($baselineHandles + 256)) {
  Write-Host "TRANSPORT SOAK WARN: handle count grew from $baselineHandles to $afterHandles"
}

if ((Invoke-Sgian @("ctl", "--workspace", $Workspace, "panes", "--json")) -ne 0) {
  throw "final ctl panes failed"
}
Write-Host "TRANSPORT SOAK OK"
