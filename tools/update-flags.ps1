# Re-downloads the country flag PNGs in res/flags (see res/flags/SOURCE.md).
#
# Usage:
#   .\tools\update-flags.ps1
#
# The flags come from flagpedia.net at 32x24; they are public domain, so they
# can ship inside the binary. build.rs turns whatever is in res/flags into the
# embedded FLAG_PNGS table, so adding or removing a file here is all it takes.

$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$root = Split-Path -Parent $PSScriptRoot
$out = Join-Path $root 'res\flags'
New-Item -ItemType Directory -Force $out | Out-Null

Write-Host "Fetching the ISO 3166-1 code list..."
$codes = (Invoke-RestMethod 'https://flagcdn.com/en/codes.json' -UserAgent 'nanotorrent-build').
    PSObject.Properties.Name | Where-Object { $_.Length -eq 2 } | Sort-Object

Write-Host "Downloading $($codes.Count) flags to res\flags ..."
$failed = @()
foreach ($c in $codes) {
    try {
        Invoke-WebRequest "https://flagcdn.com/32x24/$c.png" -OutFile (Join-Path $out "$c.png") `
            -UserAgent 'nanotorrent-build'
    } catch {
        $failed += $c
    }
}

if ($failed.Count -gt 0) {
    Write-Warning "Failed: $($failed -join ', ')"
    exit 1
}

Write-Host "Done. $($codes.Count) flags in res\flags." -ForegroundColor Green
Write-Host "Rebuild to pick them up: cargo build --release"
