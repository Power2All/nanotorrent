# Re-vendors the librqbit family from crates.io and re-applies the NanoTorrent
# patches (see ../patches and vendor/librqbit/PATCHES.md).
#
# Usage:
#   .\tools\update-librqbit.ps1              # latest stable version
#   .\tools\update-librqbit.ps1 -Version 9.1.0
#
# The vendored copies are the PUBLISHED crate sources (not the git workspace),
# so all their dependencies keep resolving from crates.io.
#
# Four crates are vendored, because several features reach past librqbit:
# patch 0005 (per-tracker announce stats) needs librqbit-tracker-comms, patches
# 0008 and 0010 (BEP 52 hash messages, fast extension) need
# librqbit-peer-protocol, and patches 0013 and 0014 (the Windows UDP reset fix
# and bind-to-interface) need librqbit-dualstack-sockets. The first three share
# librqbit's version number, so one -Version covers them; the sockets crate is
# versioned separately and is handled below.

param([string]$Version)

$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$root = Split-Path -Parent $PSScriptRoot   # nanotorrent/
$patches = Join-Path $root "patches"

# --- 1. Resolve the version -------------------------------------------------
if (-not $Version) {
    Write-Host "Querying crates.io for the latest librqbit version..."
    $info = Invoke-RestMethod "https://crates.io/api/v1/crates/librqbit" -UserAgent "nanotorrent-update-script"
    $Version = $info.crate.max_stable_version
}
Write-Host "Vendoring librqbit $Version"

$work = Join-Path $env:TEMP "librqbit-vendor-$Version"
if (Test-Path $work) { Remove-Item -Recurse -Force $work }
New-Item -ItemType Directory -Path $work | Out-Null

# --- 2. Download and extract both published crates --------------------------
# A .crate is a tar.gz; Windows 10+ ships bsdtar.
# librqbit-dualstack-sockets is versioned SEPARATELY from the librqbit family
# (0.7.0, not 9.0.1), so it is re-vendored by hand rather than by -Version.
foreach ($crate in @("librqbit", "librqbit-tracker-comms", "librqbit-peer-protocol")) {
    $crateFile = Join-Path $work "$crate.crate"
    Invoke-WebRequest "https://static.crates.io/crates/$crate/$crate-$Version.crate" `
        -OutFile $crateFile -UserAgent "nanotorrent-update-script"
    tar -xzf $crateFile -C $work
    $extracted = Join-Path $work "$crate-$Version"
    if (-not (Test-Path $extracted)) { throw "extraction failed: $extracted not found" }

    $vendor = Join-Path $root "vendor\$crate"

    # PATCHES.md lives inside vendor/librqbit and is ours, not upstream's.
    $preserve = Join-Path $vendor "PATCHES.md"
    $patchesDoc = if (Test-Path $preserve) { Get-Content $preserve -Raw } else { $null }

    if (Test-Path $vendor) { Remove-Item -Recurse -Force $vendor }
    Move-Item $extracted $vendor
    Remove-Item -Force (Join-Path $vendor ".cargo-ok"), (Join-Path $vendor ".cargo_vcs_info.json") -ErrorAction SilentlyContinue
    if ($patchesDoc) { [System.IO.File]::WriteAllText($preserve, $patchesDoc, (New-Object System.Text.UTF8Encoding $false)) }
}

# --- 3. Apply the patches ----------------------------------------------------
# Two gotchas, both of which silently corrupt the result:
#
#  * Inside a git repository, `git apply` resolves patch paths against the REPO
#    ROOT and *skips* (exit 0!) files it can't match - so run from the root with
#    --directory pointing at the right vendor folder.
#  * With core.autocrlf=true, git apply rewrites the whole patched file to CRLF
#    while its neighbours stay LF. Nothing breaks at compile time, but the next
#    `git diff` to regenerate a patch then produces a whole-file rewrite.
#    -c core.autocrlf=false keeps the crate sources as upstream published them.
#
# *-comms.patch -> vendor/librqbit-tracker-comms, *-peerproto.patch ->
# vendor/librqbit-peer-protocol, *-sockets.patch ->
# vendor/librqbit-dualstack-sockets, everything else -> vendor/librqbit.
# The suffixes have to be unmistakable - "-peer" once collided with a
# librqbit feature legitimately called "synthetic-peer".
$repoRoot = & git -C $root rev-parse --show-toplevel 2>$null
$inRepo = ($LASTEXITCODE -eq 0 -and $repoRoot)
$failed = @()
Get-ChildItem $patches -Filter "*.patch" | Sort-Object Name | ForEach-Object {
    $target = switch -Wildcard ($_.Name) {
        "*-comms.patch" { "librqbit-tracker-comms" }
        "*-peerproto.patch" { "librqbit-peer-protocol" }
        "*-sockets.patch"   { "librqbit-dualstack-sockets" }
        default         { "librqbit" }
    }
    Write-Host "Applying $($_.Name) to vendor/$target..."
    $vendor = Join-Path $root "vendor\$target"
    # cmd /c merges git's stderr without PowerShell 5.1 turning it into
    # terminating NativeCommandErrors.
    if ($inRepo) {
        $rootWin = ($repoRoot -replace '/', '\').TrimEnd('\')
        $rel = $vendor.Substring($rootWin.Length + 1) -replace '\\', '/'
        $out = & cmd /c "git -C `"$rootWin`" -c core.autocrlf=false apply --verbose --ignore-whitespace --directory=$rel -p1 `"$($_.FullName)`" 2>&1"
    } else {
        $out = & cmd /c "git -C `"$vendor`" -c core.autocrlf=false apply --verbose --ignore-whitespace -p1 `"$($_.FullName)`" 2>&1"
    }
    $out | ForEach-Object { Write-Host "  $_" }
    # "Skipped patch" also exits 0 - treat anything but a clean apply as failure.
    if ($LASTEXITCODE -ne 0 -or ($out -match "Skipped patch")) { $failed += $_.Name }
}

if ($failed.Count -gt 0) {
    Write-Host ""
    Write-Warning "These patches no longer apply cleanly against $Version :"
    $failed | ForEach-Object { Write-Warning "  $_" }
    Write-Warning "Upstream code moved - re-apply them manually (see"
    Write-Warning "vendor/librqbit/PATCHES.md for what each one does and why),"
    Write-Warning "then regenerate the .patch files against the pristine crate:"
    Write-Warning "  diff -Naur <pristine> <patched>  (rewrite the paths to a/ and b/)"
    Write-Warning "Also check whether upstream has since done the job itself - two"
    Write-Warning "patches were retired that way at the 8 -> 9 bump."
    exit 1
}

# --- 4. Bump the version in Cargo.toml ---------------------------------------
# librqbit and its sibling crates (bencode included) share one version number.
$cargoToml = Join-Path $root "Cargo.toml"
$content = Get-Content $cargoToml -Raw
$content = $content -replace 'librqbit = \{ version = "\d+\.\d+\.\d+"', "librqbit = { version = `"$Version`""
$content = $content -replace 'version = "\d+\.\d+\.\d+", package = "librqbit-bencode"', "version = `"$Version`", package = `"librqbit-bencode`""
[System.IO.File]::WriteAllText($cargoToml, $content, (New-Object System.Text.UTF8Encoding $false))

Remove-Item -Recurse -Force $work

Write-Host ""
Write-Host "Done. librqbit $Version vendored and patched." -ForegroundColor Green
Write-Host "Now run: cargo build --release && cargo test"
