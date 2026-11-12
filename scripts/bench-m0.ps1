# bench-m0.ps1 - WorktreeFS M0 benchmark harness wrapper (spec section 12).
#
# The actual harness lives in crates/wtfs-daemon/tests/m0_benchmark.rs as an
# #[ignore]d integration test: it drives the daemon in-process through the
# library API (no detached wtfsd, no named pipes) and prints a markdown
# report between "=== bench report ===" and "=== end ===" markers.
# This wrapper builds the release test binary, runs the harness, extracts the
# report, and optionally writes it to a file.
#
# Measures, for two independent clones of the same source repo:
#   1. worktree creation time (`git worktree add`)
#   2. disk usage (visible tree; for WorktreeFS also the private upper store)
#   3. `git status` latency (cold first run / hot second run)
#   4. remove time and the one-off template+mount setup cost
#
# Source repo selection:
#   -Repo <path>   clone an existing repo (e.g. the real-world dashboard repo)
#   (default)      build a repo from tests/fixtures/demo-repo
#
# Requirements: WinFSP runtime, git CLI, LIBCLANG_PATH for the winfsp-sys
# build (auto-set to the local %LOCALAPPDATA%\wtfs\tools\llvm-23.1.5\bin when
# present, or pass -LibclangPath).
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File scripts\bench-m0.ps1 [-Repo <path>]
#              [-Iterations 3] [-OutFile bench-results.md] [-Build] [-LibclangPath <dir>]

param(
    [string]$Repo = "",
    [string]$OutFile = "",
    [int]$Iterations = 3,
    [switch]$Build,
    [string]$LibclangPath = ""
)

$ErrorActionPreference = "Continue" # PS5.1: native stderr must not become terminating errors; we check $LASTEXITCODE
$ProgressPreference = "SilentlyContinue"

$script:ProjectRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path

if (-not $LibclangPath -and (Test-Path "$env:LOCALAPPDATA\wtfs\tools\llvm-23.1.5\bin")) {
    $LibclangPath = "$env:LOCALAPPDATA\wtfs\tools\llvm-23.1.5\bin"
}
if ($LibclangPath) { $env:LIBCLANG_PATH = $LibclangPath }

if ($Build) {
    Push-Location $script:ProjectRoot
    try {
        & cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build --release failed" }
    } finally { Pop-Location }
}

$env:WTFS_BENCH_ITERS = "$Iterations"
if ($Repo) {
    if (-not (Test-Path -LiteralPath $Repo)) { throw "-Repo not found: $Repo" }
    $env:WTFS_BENCH_REPO = (Resolve-Path -LiteralPath $Repo).Path
} else {
    Remove-Item Env:WTFS_BENCH_REPO -ErrorAction SilentlyContinue
}

Push-Location $script:ProjectRoot
try {
    Write-Host "building release test binary (cargo test --release --no-run)..."
    & cargo test --release -p wtfs-daemon --test m0_benchmark --no-run
    if ($LASTEXITCODE -ne 0) { throw "cargo test --release --no-run failed (exit $LASTEXITCODE)" }

    Write-Host "running M0 benchmark harness (iterations=$Iterations, repo=$(if ($Repo) { $Repo } else { 'fixture' }))..."
    $output = & cargo test --release -p wtfs-daemon --test m0_benchmark -- --ignored --nocapture 2>&1
    $exit = $LASTEXITCODE
} finally {
    Pop-Location
}

$output | ForEach-Object { $_.ToString() } | Write-Host
if ($exit -ne 0) { throw "benchmark harness failed (exit $exit)" }

$lines = @($output | ForEach-Object { $_.ToString() })
$begin = -1; $end = -1
for ($i = 0; $i -lt $lines.Count; $i++) {
    if ($begin -lt 0 -and $lines[$i].StartsWith("=== bench report ===")) { $begin = $i }
    elseif ($begin -ge 0 -and $lines[$i].StartsWith("=== end ===")) { $end = $i; break }
}
if ($begin -lt 0 -or $end -lt 0) { throw "report markers not found in harness output" }

# Stamp generation date on top of the harness report (harness stays date-free).
$report = @("- date: $((Get-Date).ToString('yyyy-MM-dd HH:mm'))") + $lines[$begin..$end]
$reportText = $report -join "`r`n"
Write-Host ""
Write-Host $reportText
if ($OutFile) {
    $outPath = if ([IO.Path]::IsPathRooted($OutFile)) { $OutFile } else { Join-Path $script:ProjectRoot $OutFile }
    Set-Content -LiteralPath $outPath -Value $reportText -Encoding UTF8
    Write-Host "`nwritten: $outPath"
}
