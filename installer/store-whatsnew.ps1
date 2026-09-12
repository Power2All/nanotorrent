# Fold the per-language listing text from MS_Store_Release_Info into a
# Microsoft Store submission.
#
#   installer\store-whatsnew.ps1 -SubmissionPath sub.json -OutPath sub.new.json
#   installer\store-whatsnew.ps1 -SubmissionPath sub.json -OutPath sub.new.json -Version 0.3.2
#   installer\store-whatsnew.ps1 -SubmissionPath sub.json -OutPath sub.new.json -NoCreate
#
# `msstore publish` only uploads the package - it never touches listing
# metadata, which is why release notes had to be pasted into Partner Center by
# hand. `msstore submission update` does take listings, but only as the whole
# submission JSON, so this reads that JSON, rewrites it and hands it back.
#
# TWO DIFFERENT JOBS, on purpose:
#
#   A language the submission ALREADY carries gets its release notes replaced
#   and nothing else. Its description, features and search terms may have been
#   edited in Partner Center since, and overwriting that from a file nobody
#   reread would quietly undo someone's work.
#
#   A language the submission does NOT carry is CREATED, in full, from its
#   listing file - description, features, search terms, copyright, licence
#   terms and all. That is the only way a newly translated language reaches the
#   Store: adding it by hand is 8 fields per language in Partner Center, and a
#   language added with any required field left empty blocks the submission.
#
# A created listing is cloned from an existing one rather than built from an
# empty object, so it inherits the fields that are not in the listing files and
# not ours to invent - the reserved app title above all, plus the privacy
# policy, website and support contact. Only the text this repo owns is then
# overwritten. `images` is the one inherited field that is emptied instead:
# image entries carry per-listing ids, and store-images.ps1 (which runs after
# this script) stages fresh ones for every listing it finds.
#
# -NoCreate falls back to the old warn-only behaviour. Keep it in reach:
# Partner Center accepts listings only in the languages it supports, and a
# locale it does not recognise fails the whole `submission update`. If that
# happens, the message names the locale - drop its .txt file, or re-run with
# -NoCreate to ship without it.
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
    [string]$ListingDir,

    # Do not add listings for languages the submission does not carry; warn
    # about them as this script used to. See the note above.
    [switch]$NoCreate
)

$ErrorActionPreference = 'Stop'

if (-not $ListingDir) {
    $ListingDir = Join-Path (Split-Path -Parent $PSScriptRoot) "MS_Store_Release_Info"
}

# Partner Center's own limits. Exceeding one is rejected at submission, long
# after the package has uploaded, so they are checked here instead.
$Limits = @{
    Description  = 10000
    ReleaseNotes = 1500
    Feature      = 200
    FeatureCount = 20
    Keyword      = 30
    KeywordCount = 7
    Copyright    = 200
    LicenseTerms = 10000
    DevStudio    = 255
}

# Section heading -> the baseListing property it feeds. The headings are the
# `--- NAME (max ...` lines in the listing files; everything between one
# heading and the next is that section's body.
$SectionFields = @{
    "DESCRIPTION"                  = 'description'
    "WHAT'S NEW IN THIS VERSION"   = 'releaseNotes'
    "PRODUCT FEATURES"             = 'features'
    "SEARCH TERMS"                 = 'keywords'
    "COPYRIGHT AND TRADEMARK INFO" = 'copyrightAndTrademarkInfo'
    "ADDITIONAL LICENSE TERMS"     = 'licenseTerms'
    "DEVELOPED BY"                 = 'devStudio'
}

# The two that are arrays of lines rather than one block of text.
$ListFields = @('features', 'keywords')

# ---------------------------------------------------------------------------
# Read the listing files
# ---------------------------------------------------------------------------

if (-not (Test-Path $ListingDir)) {
    throw "no listing folder at $ListingDir"
}

# Split one listing file into property name -> value. Headings are matched up
# to their opening parenthesis, so the character limits printed in them can be
# corrected without breaking this.
function Read-ListingFile([string]$path, [string]$name) {
    $lines = Get-Content -LiteralPath $path -Encoding utf8

    # Heading line numbers first, so each body is simply "up to the next one".
    $marks = @()
    for ($i = 0; $i -lt $lines.Count; $i++) {
        $m = [regex]::Match($lines[$i], '^---\s+(.*?)\s*\(')
        if ($m.Success) {
            $marks += [pscustomobject]@{ Line = $i; Name = $m.Groups[1].Value.Trim() }
        }
    }
    if ($marks.Count -eq 0) { throw "$name : no '--- SECTION (...' headings found" }

    $fields = @{}
    for ($k = 0; $k -lt $marks.Count; $k++) {
        $field = $SectionFields[$marks[$k].Name]
        if (-not $field) { continue }

        $from = $marks[$k].Line + 1
        if ($k + 1 -lt $marks.Count) { $to = $marks[$k + 1].Line - 1 } else { $to = $lines.Count - 1 }
        if ($to -lt $from) { throw "$name : section $($marks[$k].Name) is empty" }

        $body = ($lines[$from..$to] -join "`n").Trim()
        if (-not $body) { throw "$name : section $($marks[$k].Name) is empty" }

        if ($ListFields -contains $field) {
            # One item per line, blank lines dropped.
            $fields[$field] = @($body -split "`n" | ForEach-Object { $_.Trim() } |
                Where-Object { $_ })
        } else {
            $fields[$field] = $body
        }
    }

    foreach ($need in @('description', 'releaseNotes')) {
        if (-not $fields.ContainsKey($need)) { throw "$name : no $need section" }
    }

    # Limits, named so a failure says which field and by how much.
    function Test-Len($value, $limit, $what) {
        if ($value -and $value.Length -gt $limit) {
            throw "$name : $what is $($value.Length) characters, over the $limit limit"
        }
    }
    Test-Len $fields['description'] $Limits.Description 'description'
    Test-Len $fields['releaseNotes'] $Limits.ReleaseNotes "What's new"
    Test-Len $fields['copyrightAndTrademarkInfo'] $Limits.Copyright 'copyright info'
    Test-Len $fields['licenseTerms'] $Limits.LicenseTerms 'licence terms'
    Test-Len $fields['devStudio'] $Limits.DevStudio 'developed by'

    if ($fields['features'] -and $fields['features'].Count -gt $Limits.FeatureCount) {
        throw "$name : $($fields['features'].Count) product features, over the $($Limits.FeatureCount) limit"
    }
    foreach ($f in $fields['features']) { Test-Len $f $Limits.Feature "product feature '$f'" }

    if ($fields['keywords'] -and $fields['keywords'].Count -gt $Limits.KeywordCount) {
        throw "$name : $($fields['keywords'].Count) search terms, over the $($Limits.KeywordCount) limit"
    }
    foreach ($k in $fields['keywords']) { Test-Len $k $Limits.Keyword "search term '$k'" }

    return $fields
}

$listingFiles = @{}
foreach ($file in Get-ChildItem -Path $ListingDir -Filter *.txt | Sort-Object Name) {
    # Not a listing: it explains a store policy answer, not a language.
    if ($file.BaseName -eq 'restricted-capability-justification') { continue }

    $fields = Read-ListingFile $file.FullName $file.Name

    if ($Version -and $fields['releaseNotes'] -notmatch [regex]::Escape($Version)) {
        throw "$($file.Name): What's new does not mention version $Version - is the listing stale?"
    }

    $listingFiles[$file.BaseName] = $fields
}

if ($listingFiles.Count -eq 0) { throw "no listing files found in $ListingDir" }
Write-Host "read listing text for $($listingFiles.Count) language(s)"

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

# Assign whether or not the property is already there. A listing that has never
# carried a field has nothing to assign to, so adding is not the rare case.
function Set-Prop($object, $name, $value) {
    $prop = Get-Prop $object $name
    if ($prop) {
        $prop.Value = $value
    } else {
        $object | Add-Member -NotePropertyName $name -NotePropertyValue $value
    }
}

$listingsProp = Get-Prop $submission 'listings'
if (-not $listingsProp) {
    throw "the submission JSON has no listings - is $SubmissionPath really a submission?"
}
$listings = $listingsProp.Value

$updated = @()
$created = @()
$unmatched = @()
$storeLocales = @($listings.PSObject.Properties.Name)

foreach ($locale in $storeLocales) {
    # Partner Center returns "en-us"; the files are named "en-US". Match without
    # case, so the two spellings of one language are one language.
    $file = $listingFiles.Keys | Where-Object { $_ -ieq $locale } | Select-Object -First 1
    if (-not $file) {
        $unmatched += $locale
        continue
    }

    $baseProp = Get-Prop (Get-Prop $listings $locale).Value 'baseListing'
    if (-not $baseProp) {
        $unmatched += "$locale (no baseListing)"
        continue
    }

    # Release notes only - see the header. The rest of this language's listing
    # is whatever Partner Center currently holds.
    Set-Prop $baseProp.Value 'releaseNotes' $listingFiles[$file]['releaseNotes']
    $updated += $locale
}

# ---------------------------------------------------------------------------
# Languages with a file but no listing yet
# ---------------------------------------------------------------------------

$missing = @($listingFiles.Keys | Where-Object {
        $l = $_; -not ($storeLocales | Where-Object { $_ -ieq $l })
    } | Sort-Object)

if ($missing -and $NoCreate) {
    Write-Warning "listing files with no matching Store language: $($missing -join ', ') - left out (-NoCreate)"
} elseif ($missing) {
    # The listing to clone the non-text fields from. en-US for preference: it is
    # the one that is always present and always complete.
    $templateLocale = $storeLocales | Where-Object { $_ -ieq 'en-US' } | Select-Object -First 1
    if (-not $templateLocale) { $templateLocale = $storeLocales | Select-Object -First 1 }
    if (-not $templateLocale) {
        throw @"
the submission carries no listings at all, so there is nothing to model new
ones on. Add the first language in Partner Center by hand (Store listings >
Add a language), then re-run: every later language is created from it.
"@
    }
    $template = (Get-Prop $listings $templateLocale).Value
    Write-Host "new listings are modelled on $templateLocale"

    foreach ($locale in $missing) {
        # Round-tripping through JSON is the one deep copy available in 5.1
        # without -AsHashtable. Depth well past the listing's nesting: the
        # default of 2 would turn everything below it into strings.
        $clone = $template | ConvertTo-Json -Depth 100 | ConvertFrom-Json

        $base = (Get-Prop $clone 'baseListing').Value
        if (-not $base) { throw "$templateLocale has no baseListing to model $locale on" }

        foreach ($field in $SectionFields.Values) {
            if ($listingFiles[$locale].ContainsKey($field)) {
                Set-Prop $base $field $listingFiles[$locale][$field]
            }
        }

        # Emptied, not inherited: image entries are identified by per-listing
        # ids, and store-images.ps1 stages fresh ones for every listing.
        Set-Prop $base 'images' @()

        Set-Prop $listings $locale $clone
        $created += $locale
    }
}

# A language live in the Store with no file here keeps whatever it had, rather
# than being blanked - but say so, because it means the listing set has drifted.
if ($unmatched) {
    Write-Warning "no listing file for: $($unmatched -join ', ') - left unchanged"
}

if ($updated.Count -eq 0 -and $created.Count -eq 0) {
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
if ($created.Count -gt 0) {
    Write-Host "created $($created.Count) new listing(s): $($created -join ', ')" -ForegroundColor Green
    Write-Host "  a created listing has no screenshots yet - run store-images.ps1 on this output,"
    Write-Host "  which sets them on every language the submission carries, created ones included"
}
Write-Host "wrote $OutPath"
