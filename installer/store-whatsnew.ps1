# Fold the per-language "What's new" text from MS_Store_Release_Info into a
# Microsoft Store submission.
#
#   installer\store-whatsnew.ps1 -SubmissionPath sub.json -OutPath sub.new.json
#   installer\store-whatsnew.ps1 -SubmissionPath sub.json -OutPath sub.new.json -Version 0.3.2
#
# `msstore publish` only uploads the package - it never touches listing
# metadata, which is why release notes had to be pasted into Partner Center by
# hand. `msstore submission update` does take listings, but only as the whole
# submission JSON, so this reads that JSON, rewrites one field per language and
# hands it back.
#
# Deliberately a separate script rather than inline workflow YAML: it can be
# run against a saved `msstore submission get` dump on a laptop, with no
# credentials and no release, which is the only way to find out whether the
# language codes line up before a submission is riding on it.
#
# Runs on Windows PowerShell 5.1 as well as pwsh 7, like the other scripts
# here: the workflow uses pwsh, but a script that can only be tested inside CI
# is a script nobody tests. That rules out -AsHashtable, hence the PSObject
# navigation below.

[CmdletBinding()]
param(
    # The JSON from `msstore submission get <productId>`.
    [Parameter(Mandatory = $true)]
    [string]$SubmissionPath,

    # Where to write the updated JSON, for `msstore submission update`.
    [Parameter(Mandatory = $true)]
    [string]$OutPath,

    # If given, every listing's notes must mention this version. Guards against
    # shipping a package whose release notes still describe the last one - the
    # listing files are edited by hand and are easy to forget.
    [string]$Version,

    # Where the per-language listing files live. Defaulted in the body, not
    # here: $PSScriptRoot is not populated while 5.1 binds parameters.
    [string]$ListingDir
)

$ErrorActionPreference = 'Stop'

if (-not $ListingDir) {
    $ListingDir = Join-Path (Split-Path -Parent $PSScriptRoot) "MS_Store_Release_Info"
}

# Partner Center's own limit for this field. Exceeding it is rejected at
# submission, long after the package has uploaded.
$WhatsNewLimit = 1500

$startMarker = "--- WHAT'S NEW IN THIS VERSION"
$endMarker = '--- PRODUCT FEATURES'

# ---------------------------------------------------------------------------
# Read the listing files
# ---------------------------------------------------------------------------

if (-not (Test-Path $ListingDir)) {
    throw "no listing folder at $ListingDir"
}

$notes = @{}
foreach ($file in Get-ChildItem -Path $ListingDir -Filter *.txt | Sort-Object Name) {
    # Not a listing: it explains a store policy answer, not a language.
    if ($file.BaseName -eq 'restricted-capability-justification') { continue }

    $lines = Get-Content -LiteralPath $file.FullName -Encoding utf8
    $from = ($lines | Select-String -SimpleMatch $startMarker | Select-Object -First 1).LineNumber
    $to = ($lines | Select-String -SimpleMatch $endMarker | Select-Object -First 1).LineNumber
    if (-not $from -or -not $to -or $to -le $from) {
        throw "$($file.Name): could not find the What's new block"
    }

    # LineNumber is 1-based and both markers are excluded, so the body is the
    # lines strictly between them, trimmed of the blank padding around it.
    $body = ($lines[$from..($to - 2)] -join "`n").Trim()
    if (-not $body) { throw "$($file.Name): the What's new block is empty" }

    if ($body.Length -gt $WhatsNewLimit) {
        throw "$($file.Name): What's new is $($body.Length) characters, over the $WhatsNewLimit limit"
    }
    if ($Version -and $body -notmatch [regex]::Escape($Version)) {
        throw "$($file.Name): What's new does not mention version $Version - is the listing stale?"
    }

    $notes[$file.BaseName] = $body
}

if ($notes.Count -eq 0) { throw "no listing files found in $ListingDir" }
Write-Host "read What's new for $($notes.Count) language(s)"

# ---------------------------------------------------------------------------
# Fold them into the submission
# ---------------------------------------------------------------------------

$submission = Get-Content -LiteralPath $SubmissionPath -Raw -Encoding utf8 | ConvertFrom-Json

# Property lookups are case-insensitive throughout: the CLI has spelled these
# both ways over its life, and a casing difference is not a reason to fail.
function Get-Prop($object, $name) {
    if ($null -eq $object) { return $null }
    $object.PSObject.Properties | Where-Object { $_.Name -ieq $name } | Select-Object -First 1
}

$listingsProp = Get-Prop $submission 'listings'
if (-not $listingsProp) {
    throw "the submission JSON has no listings - is $SubmissionPath really a submission?"
}
$listings = $listingsProp.Value

$updated = @()
$unmatched = @()
$storeLocales = @($listings.PSObject.Properties.Name)

foreach ($locale in $storeLocales) {
    # Partner Center returns "en-us"; the files are named "en-US". Match without
    # case, so the two spellings of one language are one language.
    $file = $notes.Keys | Where-Object { $_ -ieq $locale } | Select-Object -First 1
    if (-not $file) {
        $unmatched += $locale
        continue
    }

    $baseProp = Get-Prop (Get-Prop $listings $locale).Value 'baseListing'
    if (-not $baseProp) {
        $unmatched += "$locale (no baseListing)"
        continue
    }

    $base = $baseProp.Value
    $notesProp = Get-Prop $base 'releaseNotes'
    if ($notesProp) {
        $notesProp.Value = $notes[$file]
    } else {
        # A listing that has never carried release notes has no property to
        # assign to, so add one rather than silently doing nothing.
        $base | Add-Member -NotePropertyName 'releaseNotes' -NotePropertyValue $notes[$file]
    }
    $updated += $locale
}

# A language live in the Store with no file here keeps whatever it had, rather
# than being blanked - but say so, because it means the listing set has drifted.
if ($unmatched) {
    Write-Warning "no listing file for: $($unmatched -join ', ') - left unchanged"
}
# The reverse is worth knowing too: a file for a language the Store does not
# carry is work being done for nobody.
$extra = $notes.Keys | Where-Object { $l = $_; -not ($storeLocales | Where-Object { $_ -ieq $l }) }
if ($extra) {
    Write-Warning "listing files with no matching Store language: $($extra -join ', ')"
}

if ($updated.Count -eq 0) {
    throw "no listing matched a language file - nothing would change, so this is a mistake not a no-op"
}

# -Depth well past the submission's nesting: the default of 2 silently turns
# everything deeper into strings, which uploads cleanly and destroys the listing.
$json = $submission | ConvertTo-Json -Depth 100 -Compress

# WriteAllText with an explicit no-BOM encoder rather than Set-Content: 5.1's
# `-Encoding utf8` writes a BOM, and a BOM in front of the JSON is not JSON as
# far as the parser on the other end is concerned.
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
# Combine then GetFullPath: Combine keeps an already-rooted path as it is, and
# GetFullPath normalises the separators. Passing the raw argument straight to
# WriteAllText fails on a forward-slash absolute path.
$outFull = [System.IO.Path]::GetFullPath(
    [System.IO.Path]::Combine((Get-Location).Path, $OutPath))
[System.IO.File]::WriteAllText($outFull, $json, $utf8NoBom)

Write-Host "updated release notes for $($updated.Count) language(s): $($updated -join ', ')"
Write-Host "wrote $OutPath"
