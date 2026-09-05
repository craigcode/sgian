param(
    [ValidateSet("Debug", "Release")]
    [string]$Configuration = "Release",
    [ValidateSet("win-x64", "win-arm64")]
    [string]$RuntimeIdentifier = "win-x64",
    [switch]$SkipRust,
    [switch]$ReleaseSigning
)

$ErrorActionPreference = "Stop"
$repo = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$platform = if ($RuntimeIdentifier -eq "win-arm64") { "ARM64" } else { "x64" }
$backendProfile = if ($Configuration -eq "Release") { "release" } else { "debug" }
$backend = Join-Path $repo "src-tauri/target/$backendProfile/sgian.exe"
if ($RuntimeIdentifier -eq "win-arm64") { $backend = Join-Path $repo "src-tauri/target/aarch64-pc-windows-msvc/$backendProfile/sgian.exe" }
$version = (Get-Content (Join-Path $repo "package.json") -Raw | ConvertFrom-Json).version
$manifestPath = Join-Path $repo "apps/windows/Sgian.Windows/Package.appxmanifest"
$originalManifest = [IO.File]::ReadAllText($manifestPath)
$certificate = $null
$removeCertificate = $false
$project = Join-Path $repo "apps/windows/Sgian.Windows/Sgian.Windows.csproj"
$buildRoot = Join-Path $repo "apps/windows/build"
$publish = Join-Path $buildRoot $RuntimeIdentifier
$packageOutput = Join-Path $buildRoot "msix"
$archive = Join-Path $buildRoot "Sgian-native-windows-$RuntimeIdentifier.zip"

Push-Location $repo
try {
    if ($ReleaseSigning) {
        if ($Configuration -ne "Release") { throw "Signing requires a Release build" }
        if (-not $env:SGIAN_WINDOWS_CERTIFICATE_PATH -or -not $env:SGIAN_WINDOWS_CERTIFICATE_PASSWORD) { throw "Windows signing credentials are required" }
        $password = ConvertTo-SecureString $env:SGIAN_WINDOWS_CERTIFICATE_PASSWORD -AsPlainText -Force
        $pfx = Get-PfxData -FilePath $env:SGIAN_WINDOWS_CERTIFICATE_PATH -Password $password
        $thumbprint = $pfx.EndEntityCertificates[0].Thumbprint
        $removeCertificate = -not (Test-Path "Cert:\CurrentUser\My\$thumbprint")
        $certificate = Import-PfxCertificate -FilePath $env:SGIAN_WINDOWS_CERTIFICATE_PATH -Password $password -CertStoreLocation Cert:\CurrentUser\My
        if (-not $certificate.HasPrivateKey) { throw "Signing certificate has no private key" }
    }
    [xml]$manifest = $originalManifest
    $manifest.Package.Identity.Version = "$version.0"
    if ($certificate) { $manifest.Package.Identity.Publisher = $certificate.Subject }
    $manifest.Save($manifestPath)
    if (-not $SkipRust) {
        $cargoArguments = @("build", "--locked", "--manifest-path", "src-tauri/Cargo.toml")
        if ($Configuration -eq "Release") { $cargoArguments += "--release" }
        if ($RuntimeIdentifier -eq "win-arm64") {
            & rustup target add aarch64-pc-windows-msvc
            if ($LASTEXITCODE -ne 0) { throw "Rust target installation failed" }
            $cargoArguments += @("--target", "aarch64-pc-windows-msvc")
        }
        & cargo @cargoArguments
        if ($LASTEXITCODE -ne 0) { throw "Rust backend build failed" }
    }
    if (-not (Test-Path $backend)) { throw "Rust backend is missing at $backend" }

    if ($ReleaseSigning) {
        $sdkRoot = Join-Path ${env:ProgramFiles(x86)} "Windows Kits/10/bin"
        $signTool = (Get-ChildItem "$sdkRoot/*/x64/signtool.exe" | Sort-Object FullName -Descending | Select-Object -First 1).FullName
        if (-not $signTool) { throw "Windows SDK signtool was not found" }
        & $signTool sign /fd SHA256 /td SHA256 /tr http://timestamp.digicert.com /sha1 $certificate.Thumbprint $backend
        if ($LASTEXITCODE -ne 0) { throw "Daemon helper signing failed" }
    }
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

    if ($ReleaseSigning) {
        foreach ($binary in @("Sgian.Windows.exe", "Sgian.Windows.dll", "Sgian.Protocol.dll")) {
            & $signTool sign /fd SHA256 /td SHA256 /tr http://timestamp.digicert.com /sha1 $certificate.Thumbprint (Join-Path $publish $binary)
            if ($LASTEXITCODE -ne 0) { throw "Native app signing failed: $binary" }
        }
    }
    if (Test-Path $packageOutput) { Remove-Item $packageOutput -Recurse -Force }
    New-Item -ItemType Directory -Force $packageOutput | Out-Null
    $signingEnabled = if ($ReleaseSigning) { "true" } else { "false" }
    $certificateThumbprint = if ($certificate) { $certificate.Thumbprint } else { "" }
    & dotnet build $project `
        -c $Configuration `
        -r $RuntimeIdentifier `
        -p:Platform=$platform `
        -p:GenerateAppxPackageOnBuild=true `
        -p:AppxPackageSigningEnabled=$signingEnabled `
        -p:PackageCertificateThumbprint=$certificateThumbprint `
        -p:AppxPackageSigningTimestampDigestAlgorithm=SHA256 `
        -p:AppxPackageSigningTimestampServerUrl=http://timestamp.digicert.com `
        -p:WindowsAppSDKSelfContained=true `
        -p:SelfContained=true `
        -p:AppxBundle=Never `
        -p:UapAppxPackageBuildMode=SideloadOnly `
        -p:AppxPackageDir="$packageOutput\" `
        -p:SgianBackendPath=$backend
    if ($LASTEXITCODE -ne 0) { throw "Native Windows MSIX build failed" }
    if ($ReleaseSigning) {
        $packages = @(Get-ChildItem $packageOutput -Recurse -Filter *.msix | Where-Object { $_.FullName -notmatch '[\\/]Dependencies[\\/]' })
        if ($packages.Count -ne 1) { throw "Expected exactly one native MSIX" }
        & $signTool verify /pa /all $packages[0].FullName
        if ($LASTEXITCODE -ne 0) { throw "MSIX signature verification failed" }
        $release = Join-Path $buildRoot "release"
        if (Test-Path $release) { Remove-Item $release -Recurse -Force }
        New-Item -ItemType Directory -Force $release | Out-Null
        $assetName = "Sgian_${version}_windows_x64.msix"
        if ($RuntimeIdentifier -eq "win-arm64") { $assetName = "Sgian_${version}_windows_arm64.msix" }
        Copy-Item $packages[0].FullName (Join-Path $release $assetName)
        $metadata = @{ version=$version; publisher=$certificate.Subject; architecture=$platform.ToLower(); asset=$assetName; sha256=(Get-FileHash (Join-Path $release $assetName) -Algorithm SHA256).Hash.ToLower() }
        $metadata | ConvertTo-Json | Set-Content (Join-Path $release "windows-package.json") -Encoding utf8
    }

    if (Test-Path $archive) { Remove-Item $archive -Force }
    Compress-Archive -Path (Join-Path $publish "*") -DestinationPath $archive -CompressionLevel Optimal
    Write-Host "Native Windows app: $publish"
    Write-Host "Portable archive: $archive"
    Get-ChildItem $packageOutput -Recurse -Include *.msix,*.msixupload | ForEach-Object {
        Write-Host "MSIX artifact: $($_.FullName)"
    }
} finally {
    [IO.File]::WriteAllText($manifestPath, $originalManifest)
    if ($removeCertificate -and $certificate) { Remove-Item "Cert:\CurrentUser\My\$($certificate.Thumbprint)" }
    Pop-Location
}
