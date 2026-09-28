<#
.SYNOPSIS
Launch (or restart) the my-as-lsp Extension Development Host.

.DESCRIPTION
Builds the LSP server (debug or release) and the VS Code extension, kills any
existing Extension Development Host, then launches a new one via `code`/`cursor`.

.PARAMETER SkipBuild
Skip both LSP build and extension compile.

.PARAMETER SkipLsp
Skip LSP build only.

.PARAMETER SkipExt
Skip extension compile only.

.PARAMETER Release
Build LSP with `cargo build --release` (default: debug). The profile is
written to lsp/target/.build-profile; the extension's dev mode reads it and
launches the server with `cargo run --release` accordingly.

.PARAMETER Target
Open any directory or .code-workspace file.
(default: paths.test_as from config/paths.local.yaml)

.EXAMPLE
tools/test-extension.ps1
Launch EDH with debug LSP, default test workspace.

.EXAMPLE
tools/test-extension.ps1 -Release -SkipExt
Launch EDH with release LSP (rebuild if stale), skip extension compile.

.EXAMPLE
tools/test-extension.ps1 -Target D:\proj\MyGame.code-workspace
Launch EDH opening a custom workspace.
#>
param(
    [switch]$SkipBuild,
    [switch]$SkipLsp,
    [switch]$SkipExt,
    [switch]$Release,
    [string]$Target = "",
    # -h shorthand (PowerShell auto-handles -? for comment-based help)
    [Alias("h")]
    [switch]$Help
)

if ($Help) {
    # Same content as -? / Get-Help: comment-based help above.
    Get-Help $PSCommandPath -Detailed
    exit 0
}

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

# ── Resolve build profile ─────────────────────────────────────────────
$BuildProfile = if ($Release) { "release" } else { "debug" }
# NOTE: [string[]] cast is required. PowerShell unwraps single-element arrays
# returned from `if` to a scalar string, which would then be splatted as a
# char array ("build" → b u i l d), causing `cargo: unexpected argument 'u'`.
[string[]]$CargoBuildArgs = if ($Release) { @("build", "--release") } else { @("build") }

# The extension's dev mode reads this marker to pick the cargo profile.
# Env vars don't survive the `code` CLI: when VS Code is already running, the
# CLI just forwards the open-window request over IPC, and the new EDH inherits
# the environment of the *running* main process, not this shell. target/ is
# gitignored and cargo leaves unknown files alone, so this is a stable side
# channel. cargo clean removes it — harmless, dev mode falls back to debug
# (and a cleaned target needs a rebuild anyway).
$ProfileMarker = Join-Path $RepoRoot "lsp" "target" ".build-profile"

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
    Write-Host "==> [1/4] Building LSP server (cargo build [$BuildProfile])..."
    Push-Location (Join-Path $RepoRoot "lsp")
    & cargo @CargoBuildArgs
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
# Write the profile marker even when the build was skipped: the caller's
# intent (-Release) is what matters, and `cargo run` rebuilds if needed.
$ProfileDir = Split-Path $ProfileMarker -Parent
if (-not (Test-Path $ProfileDir)) {
    New-Item -ItemType Directory -Path $ProfileDir -Force | Out-Null
}
Set-Content -Path $ProfileMarker -Value $BuildProfile -NoNewline

Write-Host "==> [4/4] Launching Extension Development Host ($EditorCli) [$BuildProfile]..."
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
Write-Host "==> Done! Extension Development Host launched with $LaunchTargetLabel ($BuildProfile)."
Write-Host "    Run again to restart."
