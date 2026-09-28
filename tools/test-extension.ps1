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
#                   (default: paths.test_as from config/paths.local.yaml)
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

# ── Read path config (config/paths.local.yaml) ─────────────────────────
# Schema/defaults: config/paths.example.yaml (committed). The local file is
# gitignored; tiny hand parser for the flat single-level `paths:` section.
$Paths = @{}
$ConfigFile = Join-Path $RepoRoot "config" "paths.local.yaml"
if (Test-Path $ConfigFile) {
    $inPaths = $false
    foreach ($line in Get-Content $ConfigFile) {
        if (-not $inPaths) {
            if ($line -match '^paths:\s*$') { $inPaths = $true }
            continue
        }
        if ($line -match '^\S') { break }  # top-level key: section ends
        if ($line -match '^\s*([A-Za-z0-9_]+):\s*"([^"]*)"') {
            $Paths[$Matches[1]] = $Matches[2]
        }
    }
} else {
    Write-Warning "Config not found: $ConfigFile (copy config/paths.example.yaml to paths.local.yaml)"
}

$DefaultTarget = $Paths["test_as"]
$EdhMarker = "extensionDevelopmentPath=$ExtDir"

# ── Resolve launch target ─────────────────────────────────────────────
if ($Target -ne "") {
    $LaunchTarget = $Target
    $LaunchTargetLabel = "custom ($Target)"
} else {
    if ([string]::IsNullOrWhiteSpace($DefaultTarget)) {
        Write-Error "No default target: paths.test_as is not set in config/paths.local.yaml (copy from config/paths.example.yaml), or pass -Target explicitly."
        exit 1
    }
    $LaunchTarget = $DefaultTarget
    $LaunchTargetLabel = "test workspace (paths.test_as)"
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
#
# --new-window: `code` hands the request to an already-running VS Code
# instance; if the target workspace is ALREADY open in a normal (non-EDH)
# window, VS Code focuses that window and silently ignores
# --extensionDevelopmentPath — the dev extension never loads and every
# feature appears "not working". Forcing a new window guarantees the EDH
# (with our dev extension) actually opens.
& $EditorCli "--new-window" "--extensionDevelopmentPath=$ExtDir" "$LaunchTarget"
if ($LASTEXITCODE -ne 0) {
    Write-Error "Editor CLI exited with code $LASTEXITCODE."
    exit $LASTEXITCODE
}

Write-Host ""
Write-Host "==> Done! Extension Development Host launched with $LaunchTargetLabel."
Write-Host "    Run again to restart."
