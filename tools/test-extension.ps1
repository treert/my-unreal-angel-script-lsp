#
# Launch (or restart) the my-as-lsp Extension Development Host.
#
# Usage:
#   tools/test-extension.ps1 [-SkipBuild] [-SkipLsp] [-SkipExt] [-Target <path>]
#
# -SkipBuild         Skip both LSP build and extension compile.
# -SkipLsp           Skip LSP build only.
# -SkipExt           Skip extension compile only.
#
# -Target <path>     Open any directory or .code-workspace file.
#                   (default: D:\WorkGit\UEProjs\test-as-lsp.code-workspace)
#
# NOTE on profiles: the extension's dev mode hardwires
#   `cargo run --quiet -p as-lsp` (debug). Building here only warms the
#   cargo cache so the EDH starts fast. To test a release binary, set
#   `myAngelScriptLsp.serverPath` to lsp/target/release/as-lsp.exe in the
#   target workspace settings instead.

param(
    [switch]$SkipBuild,
    [switch]$SkipLsp,
    [switch]$SkipExt,
    [string]$Target = ""
)

$ErrorActionPreference = "Stop"

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$ExtDir   = Join-Path $RepoRoot "vscode-extension"
$DefaultTarget = "D:\WorkGit\UEProjs\test-as-lsp.code-workspace"
$EdhMarker = "extensionDevelopmentPath=$ExtDir"

# ── Resolve launch target ─────────────────────────────────────────────
if ($Target -ne "") {
    $LaunchTarget = $Target
    $LaunchTargetLabel = "custom ($Target)"
} else {
    $LaunchTarget = $DefaultTarget
    $LaunchTargetLabel = "test workspace"
}

if (-not (Test-Path $LaunchTarget)) {
    Write-Error "Launch target not found: $LaunchTarget"
    exit 1
}

# Auto-detect editor CLI: prefer code, fall back to cursor
$EditorCli = $null
if (Get-Command "code" -ErrorAction SilentlyContinue) {
    $EditorCli = "code"
} elseif (Get-Command "cursor" -ErrorAction SilentlyContinue) {
    $EditorCli = "cursor"
} else {
    Write-Error "Neither 'code' nor 'cursor' CLI found in PATH."
    exit 1
}

# ── Step 1: Build LSP server ──────────────────────────────────────────
if (-not $SkipBuild -and -not $SkipLsp) {
    Write-Host "==> [1/4] Building LSP server (cargo build [debug])..."
    Push-Location (Join-Path $RepoRoot "lsp")
    & cargo build
    if ($LASTEXITCODE -ne 0) { Pop-Location; exit $LASTEXITCODE }
    Pop-Location
} else {
    Write-Host "==> [1/4] Skipping LSP build"
}

# ── Step 2: Compile extension ─────────────────────────────────────────
if (-not $SkipBuild -and -not $SkipExt) {
    Write-Host "==> [2/4] Compiling VS Code extension (npm run compile)..."
    Push-Location $ExtDir
    npm run compile
    if ($LASTEXITCODE -ne 0) { Pop-Location; exit $LASTEXITCODE }
    Pop-Location
} else {
    Write-Host "==> [2/4] Skipping extension compile"
}

# ── Step 3: Kill existing Extension Development Host ──────────────────
Write-Host "==> [3/4] Checking for existing Extension Development Host..."
# NOTE: Get-CimInstance (not the PS5-only Get-WmiObject) so this also
# works under PowerShell 7.
$edhProcesses = Get-CimInstance Win32_Process -ErrorAction SilentlyContinue |
    Where-Object { $_.CommandLine -and $_.CommandLine.Contains($EdhMarker) }

if ($edhProcesses) {
    $pids = ($edhProcesses | ForEach-Object { $_.ProcessId }) -join ", "
    Write-Host "    Found running EDH (PIDs: $pids). Terminating..."
    $edhProcesses | ForEach-Object {
        Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
    }
    Start-Sleep -Seconds 2
    Write-Host "    Previous instance terminated."
} else {
    Write-Host "    No existing instance found."
}

# ── Step 4: Launch Extension Development Host ─────────────────────────
Write-Host "==> [4/4] Launching Extension Development Host ($EditorCli) [debug]..."
Write-Host "    Extension: $ExtDir"
Write-Host "    Target ($LaunchTargetLabel): $LaunchTarget"

# NOTE: invoke the CLI directly instead of Start-Process. `code` is a .cmd
# batch wrapper; Start-Process mangles embedded quotes when handing args to
# cmd.exe, which broke workspace opening. The CLI detaches itself and
# returns immediately, so direct invocation is both correct and non-blocking.
& $EditorCli "--extensionDevelopmentPath=$ExtDir" "$LaunchTarget"
if ($LASTEXITCODE -ne 0) {
    Write-Error "Editor CLI exited with code $LASTEXITCODE."
    exit $LASTEXITCODE
}

Write-Host ""
Write-Host "==> Done! Extension Development Host launched with $LaunchTargetLabel."
Write-Host "    Run again to restart."
