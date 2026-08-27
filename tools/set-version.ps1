# Sets the application version everywhere it is written down.
#
#   tools\set-version.ps1 0.2.3 -Notes "Fixed the tray icon on Linux."
#
# Cargo.toml is the single source of truth - the About box, the peer id, the
# BEP 10 handshake string and the installer file names all derive from it. The
# rest of these files repeat it because their formats cannot read it.
#
# Kept ASCII-only so Windows PowerShell 5.1, which reads .ps1 as the system
# codepage, parses it (same constraint as installer/make-assets.ps1).

param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^\d+\.\d+\.\d+$')]
    [string]$Version,

    # One or two sentences for the AppStream release entry, which is what
    # software centres show.
    [string]$Notes = 'TODO: describe this release.',

    # Skip regenerating installer assets (needs no Windows-only tooling
    # elsewhere, but the conversion is slow and not always wanted).
    [switch]$NoInstallerAssets
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$today = Get-Date -Format 'yyyy-MM-dd'

function Edit-File($Path, $Pattern, $Replacement, $What) {
    $full = Join-Path $root $Path
    $text = Get-Content -Raw -Encoding UTF8 $full
    if ($text -notmatch $Pattern) {
        throw "$Path : could not find $What - the file has changed shape, fix set-version.ps1"
    }
    [System.IO.File]::WriteAllText($full, ($text -replace $Pattern, $Replacement))
    Write-Host "  $Path"
}

Write-Host "Setting version $Version"

# --- the source of truth --------------------------------------------------
# Anchored to the start of a line: dependencies spell their versions inline
# and [package] is the first table, so the first line-anchored match is ours.
Edit-File 'Cargo.toml' '(?m)^version = "\d+\.\d+\.\d+"' "version = `"$Version`"" '[package] version'

# --- Cargo.lock -----------------------------------------------------------
# Edited directly rather than via cargo: `cargo metadata` resolves every
# target, including iOS dependencies that are not in the local cache, so
# --offline fails and without it a version bump could pull in a dependency
# update as a side effect. Only our own [[package]] entry changes.
$lockPattern = '(?m)(^\[\[package\]\]\r?\nname = "nanotorrent"\r?\nversion = )"\d+\.\d+\.\d+"'
Edit-File 'Cargo.lock' $lockPattern "`${1}`"$Version`"" 'the nanotorrent package entry'

# --- AppStream ------------------------------------------------------------
# Newest release first; GNOME Software and KDE Discover render these.
$metainfo = Join-Path $root 'packaging\linux\org.nanotorrent.NanoTorrent.metainfo.xml'
$xml = Get-Content -Raw -Encoding UTF8 $metainfo
if ($xml -match [regex]::Escape("<release version=`"$Version`"")) {
    Write-Host "  packaging\linux\...metainfo.xml (already has $Version)"
} else {
    $entry = @"
  <releases>
    <release version="$Version" date="$today">
      <description>
        <p>$Notes</p>
      </description>
    </release>
"@
    Edit-File 'packaging\linux\org.nanotorrent.NanoTorrent.metainfo.xml' `
        '(?m)^  <releases>' $entry 'the <releases> block'
}

# --- installer assets -----------------------------------------------------
if (-not $NoInstallerAssets) {
    & (Join-Path $root 'installer\make-assets.ps1') | Out-Null
    Write-Host '  installer\version.nsh, installer.bmp, readme.rtf'
}

Write-Host ''
Write-Host 'Still needs a human:'
Write-Host '  README.md      - add a History entry saying what changed'
Write-Host '  metainfo       - check the release text reads well'
Write-Host ''
Write-Host 'Then:'
Write-Host "  git commit -am `"NanoTorrent $Version`" && git tag -a v$Version -m `"NanoTorrent $Version`""
Write-Host "  git push origin master --tags"
