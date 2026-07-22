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
Write-Host "Building muxel (ensure profiler is current)..."
Push-Location $root
cargo build -p muxel
if ($LASTEXITCODE -ne 0) { Pop-Location; exit $LASTEXITCODE }
Pop-Location

$env:MUXEL_PROFILE_TERMINAL = "1"
$env:XDG_CONFIG_HOME = Join-Path $sandbox "config"
$env:XDG_DATA_HOME = Join-Path $sandbox "data"

Write-Host "Profile ON (MUXEL_PROFILE_TERMINAL=1) — lines must start with term-prof[v2 …]"
Write-Host "If you see win= instead of Δ=, you are on a stale binary."
Write-Host "Sandbox: $sandbox"
Write-Host "Hold a key ~2s in one Claude with several Claudes visible; release; paste term-prof[v2 quiet]."
Write-Host ""

& $exe
