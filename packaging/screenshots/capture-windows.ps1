# Capture the "hero" screenshot set of usagio's menu on Windows, one per
# fixture in packaging/screenshots/fixtures/, using real usagio UI + mocked
# account data (no hand-drawn mockups).
#
# Headless render (issue #59): earlier revisions launched the live tray and
# tried to synthesize a Win+B / Enter tray-flyout open before a full-screen
# CopyFromScreen — flaky in CI (tray focus order and paint timing aren't
# guaranteed), which produced duplicate/taskbar-only frames the quality gate
# rejected. usagio now exposes `__render_shot <theme> <out.png> <scale>`, which
# rasterizes the top-level menu for a fixture's state.json straight to a PNG
# via muri's offscreen renderer — deterministic, no tray interaction. We render
# each fixture with the Windows theme and frame it onto the uniform hero
# canvas.
#
# The tray and settings variants are intentionally not produced here (see
# capture-macos.sh for the rationale — the website consumes only the menu
# variants, and neither is part of the offscreen top-level render).
#
# Requires: python (for render_fixture.py / postprocess.py + Pillow).
# Usage: capture-windows.ps1 -OutDir <output-dir>

param(
    [string]$OutDir = "."
)

$ErrorActionPreference = "Stop"
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$FixturesDir = Join-Path $ScriptDir "fixtures"
$StateDir = Join-Path $env:APPDATA "usagio"
$StateFile = Join-Path $StateDir "state.json"
$BackupFile = $null
$Theme = "windows"

# name, fixture — one output PNG per entry, each a distinct fixture so the
# within-OS SHA-uniqueness gate passes.
$Variants = @(
    @{ Name = "menu-healthy";        Fixture = "healthy" },
    @{ Name = "menu-locked";         Fixture = "weekly-locked" },
    @{ Name = "menu-session-locked"; Fixture = "session-locked" },
    @{ Name = "menu-mixed";          Fixture = "mixed" }
)

function Cleanup {
    if ($BackupFile -and (Test-Path $BackupFile)) {
        Move-Item -Force $BackupFile $StateFile
    } elseif (Test-Path $StateFile) {
        Remove-Item -Force $StateFile
    }
}

try {
    New-Item -ItemType Directory -Force -Path $StateDir | Out-Null
    if (Test-Path $StateFile) {
        $BackupFile = [System.IO.Path]::GetTempFileName()
        Copy-Item -Force $StateFile $BackupFile
    }

    $PythonCmd = Get-Command python -ErrorAction SilentlyContinue
    if (-not $PythonCmd) { $PythonCmd = Get-Command python3 -ErrorAction SilentlyContinue }
    if (-not $PythonCmd) { throw "no 'python' or 'python3' on PATH (needed for render_fixture.py / postprocess.py)" }
    $Python = $PythonCmd.Source

    # Prefer the freshly-built binary over any (possibly stale) usagio on PATH,
    # so we always render with the code under test. Get-Command returns a
    # CommandInfo (full path on .Source); normalise to a plain string.
    $UsagioBinPath = $null
    foreach ($cand in @(".\target\release\usagio.exe", ".\target\debug\usagio.exe")) {
        if (Test-Path $cand) {
            $UsagioBinPath = (Resolve-Path $cand).Path
            break
        }
    }
    if (-not $UsagioBinPath) {
        $UsagioCmd = Get-Command usagio -ErrorAction SilentlyContinue
        if ($UsagioCmd) { $UsagioBinPath = $UsagioCmd.Source }
    }
    if (-not $UsagioBinPath) {
        throw "could not find a built usagio.exe (checked target\release, target\debug, PATH)"
    }

    foreach ($v in $Variants) {
        & $Python (Join-Path $ScriptDir "render_fixture.py") (Join-Path $FixturesDir "$($v.Fixture).json") $StateFile

        $raw = Join-Path $OutDir "windows-$($v.Name)-raw.png"
        & $UsagioBinPath "__render_shot" $Theme $raw "2"
        if ($LASTEXITCODE -ne 0) { throw "__render_shot failed for $($v.Name) (exit $LASTEXITCODE)" }

        & $Python (Join-Path $ScriptDir "postprocess.py") $raw (Join-Path $OutDir "windows-$($v.Name).png") `
            --frame --bg "236,236,238"
        Remove-Item -Force $raw -ErrorAction SilentlyContinue
    }

    Write-Output "Wrote $($Variants.Count) Windows hero screenshots to $OutDir"
} finally {
    Cleanup
}
