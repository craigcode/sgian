param(
    [ValidateSet("Debug", "Release")]
    [string]$Configuration = "Release",
    [ValidateSet("win-x64", "win-arm64")]
    [string]$RuntimeIdentifier = "win-x64",
    [switch]$SkipRust,
    [switch]$ReleaseSigning,
    # pfx: import SGIAN_WINDOWS_CERTIFICATE_PATH into the user store and sign
    # with its thumbprint (the MSIX is signed by MSBuild). trusted: sign through
    # Azure Trusted Signing's signtool plug-in with the identity already logged
    # in to Azure; no certificate file exists, so the MSIX publisher comes from
    # SGIAN_WINDOWS_PUBLISHER and the MSIX is signed after packaging.
    [ValidateSet("pfx", "trusted")]
    [string]$SigningMode = "pfx"
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
$publisher = $null
$signFile = $null
$trustedMetadata = $null
$project = Join-Path $repo "apps/windows/Sgian.Windows/Sgian.Windows.csproj"
$buildRoot = Join-Path $repo "apps/windows/build"
$publish = Join-Path $buildRoot $RuntimeIdentifier
$packageOutput = Join-Path $buildRoot "msix"
$archive = Join-Path $buildRoot "Sgian-native-windows-$RuntimeIdentifier.zip"

Push-Location $repo
try {
    & npm ci --ignore-scripts
    if ($LASTEXITCODE -ne 0) { throw "Frontend dependency installation failed" }
    & npm run frontend:build
    if ($LASTEXITCODE -ne 0) { throw "Frontend build failed" }
    if ($ReleaseSigning) {
        if ($Configuration -ne "Release") { throw "Signing requires a Release build" }
        $sdkRoot = Join-Path ${env:ProgramFiles(x86)} "Windows Kits/10/bin"
        $signTool = (Get-ChildItem "$sdkRoot/*/x64/signtool.exe" | Sort-Object FullName -Descending | Select-Object -First 1).FullName
        if (-not $signTool) { throw "Windows SDK signtool was not found" }
        if ($SigningMode -eq "pfx") {
            if (-not $env:SGIAN_WINDOWS_CERTIFICATE_PATH -or -not $env:SGIAN_WINDOWS_CERTIFICATE_PASSWORD) { throw "Windows signing credentials are required" }
            $password = ConvertTo-SecureString $env:SGIAN_WINDOWS_CERTIFICATE_PASSWORD -AsPlainText -Force
            $pfx = Get-PfxData -FilePath $env:SGIAN_WINDOWS_CERTIFICATE_PATH -Password $password
            $thumbprint = $pfx.EndEntityCertificates[0].Thumbprint
            $removeCertificate = -not (Test-Path "Cert:\CurrentUser\My\$thumbprint")
            Import-PfxCertificate -FilePath $env:SGIAN_WINDOWS_CERTIFICATE_PATH -Password $password -CertStoreLocation Cert:\CurrentUser\My | Out-Null
            $certificate = Get-Item "Cert:\CurrentUser\My\$thumbprint"
            if (-not $certificate.HasPrivateKey) { throw "Signing certificate has no private key" }
            $publisher = $certificate.Subject
            $signFile = {
                param($path)
                & $signTool sign /fd SHA256 /td SHA256 /tr http://timestamp.digicert.com /sha1 $certificate.Thumbprint $path
                if ($LASTEXITCODE -ne 0) { throw "Signing failed: $path" }
            }
        } else {
            foreach ($name in @("SGIAN_TRUSTED_SIGNING_ENDPOINT", "SGIAN_TRUSTED_SIGNING_ACCOUNT", "SGIAN_TRUSTED_SIGNING_PROFILE", "SGIAN_TRUSTED_SIGNING_DLIB", "SGIAN_WINDOWS_PUBLISHER")) {
                if (-not (Get-Item "env:$name" -ErrorAction SilentlyContinue).Value) { throw "Trusted Signing requires $name" }
            }
            if (-not (Test-Path $env:SGIAN_TRUSTED_SIGNING_DLIB)) { throw "Trusted Signing plug-in not found at $env:SGIAN_TRUSTED_SIGNING_DLIB" }
            if ($env:SGIAN_WINDOWS_PUBLISHER -notmatch '^CN=') { throw "SGIAN_WINDOWS_PUBLISHER must be the certificate subject, starting with CN=" }
            $publisher = $env:SGIAN_WINDOWS_PUBLISHER
            $trustedMetadata = Join-Path $buildRoot "trusted-signing.json"
            New-Item -ItemType Directory -Force $buildRoot | Out-Null
            @{ Endpoint = $env:SGIAN_TRUSTED_SIGNING_ENDPOINT; CodeSigningAccountName = $env:SGIAN_TRUSTED_SIGNING_ACCOUNT; CertificateProfileName = $env:SGIAN_TRUSTED_SIGNING_PROFILE } |
                ConvertTo-Json | Set-Content $trustedMetadata -Encoding utf8
            $dlib = $env:SGIAN_TRUSTED_SIGNING_DLIB
            $signFile = {
                param($path)
                & $signTool sign /fd SHA256 /td SHA256 /tr http://timestamp.acs.microsoft.com /dlib $dlib /dmdf $trustedMetadata $path
                if ($LASTEXITCODE -ne 0) { throw "Trusted Signing failed: $path" }
            }
        }
    }
    [xml]$manifest = $originalManifest
    $manifest.Package.Identity.Version = "$version.0"
    if ($publisher) { $manifest.Package.Identity.Publisher = $publisher }
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

    if ($ReleaseSigning) { & $signFile $backend }
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
            & $signFile (Join-Path $publish $binary)
        }
    }
    if (Test-Path $packageOutput) { Remove-Item $packageOutput -Recurse -Force }
    New-Item -ItemType Directory -Force $packageOutput | Out-Null
    # MSBuild signs the MSIX only when a certificate is in the store (pfx).
    # Under Trusted Signing the package is built unsigned and signed below.
    $signingEnabled = if ($ReleaseSigning -and $certificate) { "true" } else { "false" }
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
        if (-not $certificate) { & $signFile $packages[0].FullName }
        & $signTool verify /pa /all $packages[0].FullName
        if ($LASTEXITCODE -ne 0) { throw "MSIX signature verification failed" }
        $release = Join-Path $buildRoot "release"
        if (Test-Path $release) { Remove-Item $release -Recurse -Force }
        New-Item -ItemType Directory -Force $release | Out-Null
        $assetName = "Sgian_${version}_windows_x64.msix"
        if ($RuntimeIdentifier -eq "win-arm64") { $assetName = "Sgian_${version}_windows_arm64.msix" }
        Copy-Item $packages[0].FullName (Join-Path $release $assetName)
        $metadata = @{ version=$version; publisher=$publisher; architecture=$platform.ToLower(); asset=$assetName; sha256=(Get-FileHash (Join-Path $release $assetName) -Algorithm SHA256).Hash.ToLower() }
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
    if ($trustedMetadata -and (Test-Path $trustedMetadata)) { Remove-Item $trustedMetadata -Force }
    Pop-Location
}
