# Re-vendors librqbit from crates.io and re-applies the NanoTorrent
# visibility patches (see ../patches and vendor/librqbit/PATCHES.md).
#
# Usage:
#   .\tools\update-librqbit.ps1              # latest stable version
#   .\tools\update-librqbit.ps1 -Version 8.2.0
#
# The vendored copy is the PUBLISHED crate source (not the git workspace),
# so all of librqbit's dependencies keep resolving from crates.io.

param([string]$Version)

$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$root = Split-Path -Parent $PSScriptRoot   # picotorrent-rust/
$vendor = Join-Path $root "vendor\librqbit"
$patches = Join-Path $root "patches"

# --- 1. Resolve the version -------------------------------------------------
if (-not $Version) {
    Write-Host "Querying crates.io for the latest librqbit version..."
    $info = Invoke-RestMethod "https://crates.io/api/v1/crates/librqbit" -UserAgent "nanotorrent-update-script"
    $Version = $info.crate.max_stable_version
}
Write-Host "Vendoring librqbit $Version"

# --- 2. Download and extract the published crate ----------------------------
$work = Join-Path $env:TEMP "librqbit-vendor-$Version"
if (Test-Path $work) { Remove-Item -Recurse -Force $work }
New-Item -ItemType Directory -Path $work | Out-Null

$crateFile = Join-Path $work "librqbit.crate"
Invoke-WebRequest "https://static.crates.io/crates/librqbit/librqbit-$Version.crate" `
    -OutFile $crateFile -UserAgent "nanotorrent-update-script"

# A .crate is a tar.gz; Windows 10+ ships bsdtar.
tar -xzf $crateFile -C $work
$extracted = Join-Path $work "librqbit-$Version"
if (-not (Test-Path $extracted)) { throw "extraction failed: $extracted not found" }

# --- 3. Replace the vendored copy -------------------------------------------
$preserve = Join-Path $vendor "PATCHES.md"
$patchesDoc = if (Test-Path $preserve) { Get-Content $preserve -Raw } else { $null }

if (Test-Path $vendor) { Remove-Item -Recurse -Force $vendor }
Move-Item $extracted $vendor
Remove-Item -Force (Join-Path $vendor ".cargo-ok"), (Join-Path $vendor ".cargo_vcs_info.json") -ErrorAction SilentlyContinue
if ($patchesDoc) { [System.IO.File]::WriteAllText($preserve, $patchesDoc, (New-Object System.Text.UTF8Encoding $false)) }

# --- 4. Apply the patches ----------------------------------------------------
# GOTCHA: inside a git repository, `git apply` resolves patch paths against
# the REPO ROOT and silently *skips* (exit 0!) files it can't match - so we
# must run from the root with --directory pointing at the vendor folder.
$repoRoot = & git -C $vendor rev-parse --show-toplevel 2>$null
$inRepo = ($LASTEXITCODE -eq 0 -and $repoRoot)
$failed = @()
Get-ChildItem $patches -Filter "*.patch" | Sort-Object Name | ForEach-Object {
    Write-Host "Applying $($_.Name)..."
    # cmd /c merges git's stderr without PowerShell 5.1 turning it into
    # terminating NativeCommandErrors.
    if ($inRepo) {
        $rootWin = ($repoRoot -replace '/', '\').TrimEnd('\')
        $rel = $vendor.Substring($rootWin.Length + 1) -replace '\\', '/'
        $out = & cmd /c "git -C `"$rootWin`" apply --verbose --ignore-whitespace --directory=$rel -p1 `"$($_.FullName)`" 2>&1"
    } else {
        $out = & cmd /c "git -C `"$vendor`" apply --verbose --ignore-whitespace -p1 `"$($_.FullName)`" 2>&1"
    }
    $out | ForEach-Object { Write-Host "  $_" }
    # "Skipped patch" also exits 0 - treat anything but a clean apply as failure.
    if ($LASTEXITCODE -ne 0 -or ($out -match "Skipped patch")) { $failed += $_.Name }
}

if ($failed.Count -gt 0) {
    Write-Host ""
    Write-Warning "These patches no longer apply cleanly against $Version :"
    $failed | ForEach-Object { Write-Warning "  $_" }
    Write-Warning "Upstream code moved - re-apply them manually (they are tiny; see"
    Write-Warning "vendor/librqbit/PATCHES.md for what each one does), then regenerate"
    Write-Warning "the .patch files with: git diff --no-index <pristine> <patched>"
    exit 1
}

# --- 5. Bump the version in Cargo.toml ---------------------------------------
$cargoToml = Join-Path $root "Cargo.toml"
$content = Get-Content $cargoToml -Raw
$content = $content -replace 'librqbit = "\d+\.\d+\.\d+"', "librqbit = `"$Version`""
[System.IO.File]::WriteAllText($cargoToml, $content, (New-Object System.Text.UTF8Encoding $false))

Remove-Item -Recurse -Force $work

Write-Host ""
Write-Host "Done. librqbit $Version vendored and patched." -ForegroundColor Green
Write-Host "Now run: cargo build --release && cargo test"
