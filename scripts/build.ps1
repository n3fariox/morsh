$ErrorActionPreference = "Stop"

(&mise activate pwsh) | Out-String | Invoke-Expression

$Root = Split-Path -Parent $MyInvocation.MyCommand.Path | Split-Path -Parent
$GhosttyDir = Join-Path $Root "third_party/ghostty"

if (-not (Test-Path (Join-Path $GhosttyDir "build.zig"))) {
    Write-Host "Initializing ghostty submodule..."
    Push-Location $Root
    try {
        git submodule update --init --recursive third_party/ghostty
    } finally {
        Pop-Location
    }
}

if (-not (Test-Path (Join-Path $GhosttyDir "build.zig"))) {
    Write-Error "Ghostty submodule missing at $GhosttyDir"
    exit 1
}

$GhosttySourceDir = (Resolve-Path $GhosttyDir).Path
Write-Host "GHOSTTY_SOURCE_DIR=$GhosttySourceDir"

Push-Location $Root
try {
    $env:GHOSTTY_SOURCE_DIR = $GhosttySourceDir
    cargo build --workspace $args
} finally {
    Pop-Location
}
