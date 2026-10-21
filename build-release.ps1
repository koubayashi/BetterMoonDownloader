# Builds the release exe and installer without local paths baked in.
#
# Rust records where each crate was compiled so panics can name the source
# file. Left alone, that embeds C:\Users\<you>\.cargo\... and this checkout's
# location in the exe. These remaps rewrite those prefixes to neutral names.
# Nothing personal is in this script: every path is read from the machine.
#
# Usage:  powershell -ExecutionPolicy Bypass -File .\build-release.ps1

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE ".cargo" }
$rustupHome = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $env:USERPROFILE ".rustup" }

# When several prefixes match, rustc applies the last one: general first.
$remaps = @(
    "$env:USERPROFILE=~",
    "$root=bmd",
    "$rustupHome=rustup",
    "$cargoHome=cargo"
) | ForEach-Object { "--remap-path-prefix=$_" }

# CARGO_ENCODED_RUSTFLAGS is separated by 0x1F, so paths with spaces are safe.
$env:CARGO_ENCODED_RUSTFLAGS = $remaps -join [char]0x1F
Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue

Push-Location (Join-Path $root "src-tauri")
try {
    npx --yes @tauri-apps/cli@2 build --bundles nsis
    if ($LASTEXITCODE -ne 0) { throw "tauri build failed" }
} finally {
    Pop-Location
}

$exe = Join-Path $root "src-tauri\target\release\better-moon-downloader.exe"
$leaks = Select-String -Path $exe -Pattern ([regex]::Escape($env:USERPROFILE)), ([regex]::Escape($root)) -SimpleMatch:$false -Encoding Default -List
if ($leaks) { throw "Local paths are still embedded in $exe" }
Write-Host "Clean build: no local paths embedded."
