param(
    [ValidateSet("Debug", "Release")]
    [string]$Configuration = "Release",
    [ValidateSet("win-x64", "win-arm64")]
    [string]$RuntimeIdentifier = "win-x64",
    [switch]$SkipRust
)

$ErrorActionPreference = "Stop"
$repo = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$platform = if ($RuntimeIdentifier -eq "win-arm64") { "ARM64" } else { "x64" }
$backendProfile = if ($Configuration -eq "Release") { "release" } else { "debug" }
$backend = Join-Path $repo "src-tauri/target/$backendProfile/sgian.exe"
$project = Join-Path $repo "apps/windows/Sgian.Windows/Sgian.Windows.csproj"
$buildRoot = Join-Path $repo "apps/windows/build"
$publish = Join-Path $buildRoot $RuntimeIdentifier
$packageOutput = Join-Path $buildRoot "msix"
$archive = Join-Path $buildRoot "Sgian-native-windows-$RuntimeIdentifier.zip"

Push-Location $repo
try {
    if (-not $SkipRust) {
        $cargoArguments = @("build", "--manifest-path", "src-tauri/Cargo.toml")
        if ($Configuration -eq "Release") { $cargoArguments += "--release" }
        & cargo @cargoArguments
        if ($LASTEXITCODE -ne 0) { throw "Rust backend build failed" }
    }
    if (-not (Test-Path $backend)) { throw "Rust backend is missing at $backend" }

    & "$PSScriptRoot/verify-windows-native.ps1" -Configuration $Configuration

    if (Test-Path $publish) { Remove-Item $publish -Recurse -Force }
    New-Item -ItemType Directory -Force $publish | Out-Null
    & dotnet publish $project `
        -c $Configuration `
        -r $RuntimeIdentifier `
        --self-contained true `
        -p:Platform=$platform `
        -p:WindowsPackageType=None `
        -p:WindowsAppSDKSelfContained=true `
        -p:SgianBackendPath=$backend `
        -o $publish
    if ($LASTEXITCODE -ne 0) { throw "Unpackaged native Windows publish failed" }

    New-Item -ItemType Directory -Force $packageOutput | Out-Null
    & dotnet build $project `
        -c $Configuration `
        -r $RuntimeIdentifier `
        -p:Platform=$platform `
        -p:GenerateAppxPackageOnBuild=true `
        -p:AppxPackageSigningEnabled=false `
        -p:AppxBundle=Never `
        -p:UapAppxPackageBuildMode=SideloadOnly `
        -p:AppxPackageDir="$packageOutput\" `
        -p:SgianBackendPath=$backend
    if ($LASTEXITCODE -ne 0) { throw "Native Windows MSIX build failed" }

    if (Test-Path $archive) { Remove-Item $archive -Force }
    Compress-Archive -Path (Join-Path $publish "*") -DestinationPath $archive -CompressionLevel Optimal
    Write-Host "Native Windows app: $publish"
    Write-Host "Portable archive: $archive"
    Get-ChildItem $packageOutput -Recurse -Include *.msix,*.msixupload | ForEach-Object {
        Write-Host "MSIX artifact: $($_.FullName)"
    }
} finally {
    Pop-Location
}
