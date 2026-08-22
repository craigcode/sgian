# ENHANCEMENTS §5: launch packaged Sgian GUI under SGIAN_UI_SMOKE.
param(
  [Parameter(Mandatory = $true)][string]$Exe,
  [Parameter(Mandatory = $true)][string]$Workspace,
  [string]$Marker = "",
  [int]$TimeoutSecs = 90
)

$ErrorActionPreference = "Stop"
New-Item -ItemType Directory -Force -Path $Workspace | Out-Null
if (-not $Marker) {
  $Marker = Join-Path $Workspace ".sgian-ui-smoke-ok"
}
$errPath = "${Marker}.err"
Remove-Item -Force -ErrorAction SilentlyContinue $Marker, $errPath

$env:SGIAN_UI_SMOKE = "1"
$env:SGIAN_WORKSPACE = $Workspace
$env:SGIAN_UI_SMOKE_MARKER = $Marker

$proc = Start-Process -FilePath $Exe -PassThru
$deadline = (Get-Date).AddSeconds($TimeoutSecs)
while ((Get-Date) -lt $deadline) {
  if (Test-Path $Marker) {
    Write-Host "UI SMOKE OK ($Marker)"
    try { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue } catch {}
    Get-Process -Name "sgian" -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    exit 0
  }
  if (Test-Path $errPath) {
    Write-Host "UI SMOKE FAILED: $(Get-Content -Raw $errPath)"
    try { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue } catch {}
    exit 1
  }
  if ($proc.HasExited) {
    # The smoke command writes its marker immediately before process exit.
    # Re-check both files after observing exit so the polling order cannot
    # turn a successful run into a false failure.
    if (Test-Path $Marker) {
      Write-Host "UI SMOKE OK ($Marker)"
      exit 0
    }
    if (Test-Path $errPath) {
      Write-Host "UI SMOKE FAILED: $(Get-Content -Raw $errPath)"
      exit 1
    }
    Write-Host "UI SMOKE FAILED: app exited before writing marker (exit $($proc.ExitCode))"
    exit 1
  }
  Start-Sleep -Seconds 1
}

Write-Host "UI SMOKE TIMED OUT after ${TimeoutSecs}s"
try { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue } catch {}
exit 1
