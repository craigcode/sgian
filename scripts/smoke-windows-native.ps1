param(
    [Parameter(Mandatory = $true)]
    [string]$Exe,
    [Parameter(Mandatory = $true)]
    [string]$Workspace,
    [Parameter(Mandatory = $true)]
    [string]$Marker,
    [int]$TimeoutSecs = 120
)

$ErrorActionPreference = "Stop"
$resolvedExe = (Resolve-Path $Exe).Path
New-Item -ItemType Directory -Force $Workspace | Out-Null
Remove-Item $Marker -Force -ErrorAction SilentlyContinue
Remove-Item "$Marker.err" -Force -ErrorAction SilentlyContinue
Remove-Item "$Marker.trace" -Force -ErrorAction SilentlyContinue

$env:SGIAN_UI_SMOKE = "1"
$env:SGIAN_UI_SMOKE_MARKER = $Marker
$env:SGIAN_WORKSPACE = $Workspace
$process = $null
$started = Get-Date

function Write-NativeDiagnostics {
    if (Test-Path "$Marker.trace") {
        Write-Host "=== Native Windows startup trace ==="
        Get-Content "$Marker.trace" | Write-Host
    }
    Write-Host "=== Recent Windows application events ==="
    Get-WinEvent -FilterHashtable @{ LogName = "Application"; StartTime = $started } -ErrorAction SilentlyContinue |
        Where-Object {
            $_.ProviderName -in @("Application Error", ".NET Runtime", "Windows Error Reporting") -or
            $_.Message -match "Sgian\.Windows"
        } |
        Select-Object TimeCreated, ProviderName, Id, LevelDisplayName, Message |
        Format-List | Out-String | Write-Host
    $werRoots = @(
        (Join-Path $env:LOCALAPPDATA "Microsoft\Windows\WER\ReportArchive"),
        (Join-Path $env:LOCALAPPDATA "Microsoft\Windows\WER\ReportQueue")
    )
    foreach ($root in $werRoots) {
        if (-not (Test-Path $root)) { continue }
        Get-ChildItem $root -Recurse -Filter Report.wer -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match "Sgian\.Windows" } |
            ForEach-Object {
                Write-Host "=== $($_.FullName) ==="
                Get-Content $_.FullName | Write-Host
            }
    }
}

try {
    $process = Start-Process -FilePath $resolvedExe -PassThru
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSecs)
    while ([DateTime]::UtcNow -lt $deadline) {
        if (Test-Path "$Marker.err") {
            Write-NativeDiagnostics
            throw "Native Windows UI smoke failed: $(Get-Content "$Marker.err" -Raw)"
        }
        if (Test-Path $Marker) {
            Write-Host "NATIVE WINDOWS UI SMOKE OK"
            return
        }
        if ($process.HasExited) {
            Start-Sleep -Seconds 2
            Write-NativeDiagnostics
            throw "Native Windows UI exited before writing its smoke marker (exit $($process.ExitCode))"
        }
        Start-Sleep -Milliseconds 250
    }
    Write-NativeDiagnostics
    throw "Native Windows UI smoke timed out after $TimeoutSecs seconds"
} finally {
    if ($process -and -not $process.HasExited) {
        Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
    }
    $backend = Join-Path (Split-Path $resolvedExe -Parent) "Helpers/sgian.exe"
    if (Test-Path $backend) {
        $shutdown = Start-Process -FilePath $backend `
            -ArgumentList @("ctl", "--workspace", $Workspace, "shutdown") `
            -Wait -PassThru -NoNewWindow
        if ($shutdown.ExitCode -ne 0) {
            Write-Warning "Native smoke daemon shutdown returned $($shutdown.ExitCode)"
        }
    }
    Remove-Item Env:SGIAN_UI_SMOKE -ErrorAction SilentlyContinue
    Remove-Item Env:SGIAN_UI_SMOKE_MARKER -ErrorAction SilentlyContinue
    Remove-Item Env:SGIAN_WORKSPACE -ErrorAction SilentlyContinue
}
