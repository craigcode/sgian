param(
    [string]$Configuration = "Release"
)

$ErrorActionPreference = "Stop"
$repo = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path

Push-Location $repo
try {
    & cargo test --manifest-path src-tauri/Cargo.toml native_ipc_discovery_exposes_location_but_never_token
    if ($LASTEXITCODE -ne 0) { throw "Rust native endpoint test failed" }

    & dotnet run --project apps/windows/Sgian.Protocol.Tests/Sgian.Protocol.Tests.csproj -c $Configuration
    if ($LASTEXITCODE -ne 0) { throw "Sgian.Protocol checks failed" }
} finally {
    Pop-Location
}
