# Launch a sandboxed muxel with terminal main-thread profiling.
# Does not touch the real workspace or kill other muxel processes.
#
# Usage (from repo root):
#   pwsh -File scripts/profile-terminal.ps1
#
# Watch stderr for lines starting with `term-prof`. Hold a key in a terminal
# for ~2s, release, wait for a `term-prof[quiet]` line.

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$sandbox = Join-Path $root ".muxel-lagtest"
New-Item -ItemType Directory -Force -Path (Join-Path $sandbox "config"), (Join-Path $sandbox "data") | Out-Null

$exe = Join-Path $root "target\debug\muxel.exe"
if (-not (Test-Path $exe)) {
    Write-Host "Building muxel..."
    Push-Location $root
    cargo build -p muxel
    Pop-Location
}

$env:MUXEL_PROFILE_TERMINAL = "1"
$env:XDG_CONFIG_HOME = Join-Path $sandbox "config"
$env:XDG_DATA_HOME = Join-Path $sandbox "data"

Write-Host "Profile ON (MUXEL_PROFILE_TERMINAL=1)"
Write-Host "Sandbox: $sandbox"
Write-Host "stderr lines: term-prof[tick] every ~500ms while active; term-prof[quiet] ~1s after last event"
Write-Host "Hold a key in the focused terminal, release, paste the quiet line back."
Write-Host ""

& $exe
