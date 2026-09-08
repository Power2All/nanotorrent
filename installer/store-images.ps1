# Put the screenshots in `images/` into every language of a Microsoft Store
# submission.
#
#   installer\store-images.ps1 -SubmissionPath sub.json -OutPath sub.new.json
#   installer\store-images.ps1 -SubmissionPath sub.json -OutPath sub.new.json -ZipPath images.zip
#   installer\store-images.ps1 -SubmissionPath sub.json -OutPath sub.new.json -Upload
#
# The companion to store-whatsnew.ps1, and built the same way: it runs against
# a saved `msstore submission get` dump on a laptop, with no credentials and no
# release, because a script that can only be tested inside a real submission is
# a script nobody tests.
#
# ---------------------------------------------------------------------------
# WHY THIS IS NOT JUST A JSON EDIT
#
# Listing text is only JSON, so store-whatsnew.ps1 can do its whole job by
# rewriting a field. Images are not: the submission carries only their NAMES,
# and the bytes have to arrive separately, inside the ZIP archive that is
# uploaded to the submission's `fileUploadUrl` - the same archive that carries
# the .msixupload.
#
# `msstore` has no command for that. Its submission verbs are status, get,
# getListingAssets, updateMetadata, update, poll, publish, delete and rollout;
# `getListingAssets` READS the assets, and nothing writes image bytes. So
# adding a screenshot cannot be done through the CLI at all, and a submission
# whose JSON names a file that is not in the archive fails to commit with
# MissingFiles.
#
# What this script does about it:
#
#   1. Rewrites every language's `images` array to name the screenshots.
#   2. Writes those screenshots into a ZIP, laid out the way the JSON names
#      them.
#   3. With -Upload, merges that ZIP into the archive `msstore publish` already
#      uploaded and puts it back, so the package and the images travel together
#      as the API requires.
#
# Step 3 is the one that needs a real submission: it reads `fileUploadUrl` out
# of the dump, and that URL is a short-lived SAS. Steps 1 and 2 need nothing
# but the dump and the PNGs, which is what makes the whole thing testable
# before a release is riding on it.
#
# ---------------------------------------------------------------------------
# Runs on Windows PowerShell 5.1 as well as pwsh 7, like the other scripts
# here, so: no ternaries, no ??, no -AsHashtable, and PSObject navigation
# rather than hashtable indexing.

[CmdletBinding()]
param(
    # The JSON from `msstore submission get <productId>`.
    [Parameter(Mandatory = $true)]
    [string]$SubmissionPath,

    # Where to write the updated JSON, for `msstore submission update`.
    [Parameter(Mandatory = $true)]
    [string]$OutPath,

    # Where the screenshots live. Defaulted in the body, not here:
    # $PSScriptRoot is not populated while 5.1 binds parameters.
    [string]$ImageDir,

    # Where to write the ZIP of screenshots. Written whenever given, and
    # required by -Upload.
    [string]$ZipPath,

    # Merge the ZIP into the archive already uploaded for this submission and
    # put it back. Needs `fileUploadUrl` in the dump and a SAS that has not
    # expired, so this is the step that only works against a live draft.
    [switch]$Upload
)

$ErrorActionPreference = 'Stop'

if (-not $ImageDir) {
    $ImageDir = Join-Path (Split-Path -Parent $PSScriptRoot) "images"
}
if ($Upload -and -not $ZipPath) {
    $ZipPath = Join-Path ([System.IO.Path]::GetTempPath()) "store-images.zip"
}

# The folder the images sit in INSIDE the archive. A folder rather than the
# root so that merging into the package archive cannot collide with a file the
# packaging step put there.
$ZipFolder = 'listing-images'

# Microsoft's rules for a desktop screenshot. Checked here because the
# alternative is finding out days later, in a certification failure that names
# the file and nothing else.
$MinWidth = 1366
$MinHeight = 768
$MaxScreenshots = 10
$MaxBytes = 50MB

# ---------------------------------------------------------------------------
# Read and check the screenshots
# ---------------------------------------------------------------------------

if (-not (Test-Path $ImageDir)) {
    throw "no image folder at $ImageDir"
}

# Sorted by name, which is why they are numbered: the Store shows them in the
# order the array gives, and that order is the first thing a visitor sees.
$files = @(Get-ChildItem -Path $ImageDir -Filter *.png | Sort-Object Name)
if ($files.Count -eq 0) {
    throw "no .png screenshots in $ImageDir - the Store does not take .webp"
}
if ($files.Count -gt $MaxScreenshots) {
    throw "$($files.Count) screenshots in $ImageDir, but a listing takes at most $MaxScreenshots"
}

Add-Type -AssemblyName System.Drawing

$images = @()
foreach ($file in $files) {
    if ($file.Length -gt $MaxBytes) {
        throw "$($file.Name) is $([math]::Round($file.Length / 1MB, 1)) MB, over the 50 MB limit"
    }

    $bitmap = [System.Drawing.Image]::FromFile($file.FullName)
    try {
        $w = $bitmap.Width
        $h = $bitmap.Height
    } finally {
        $bitmap.Dispose()
    }

    if ($w -lt $MinWidth -or $h -lt $MinHeight) {
        throw "$($file.Name) is ${w}x${h}, under the ${MinWidth}x${MinHeight} minimum"
    }

    $images += [pscustomobject]@{
        Path     = $file.FullName
        # Forward slashes: this is a path inside a ZIP, not a Windows path.
        ZipName  = "$ZipFolder/$($file.Name)"
        Width    = $w
        Height   = $h
    }
    Write-Host ("  {0,-32} {1}x{2}" -f $file.Name, $w, $h)
}
Write-Host "$($images.Count) screenshot(s) accepted"

# ---------------------------------------------------------------------------
# Rewrite the listings
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
foreach ($locale in @($listings.PSObject.Properties.Name)) {
    $baseProp = Get-Prop (Get-Prop $listings $locale).Value 'baseListing'
    if (-not $baseProp) {
        Write-Warning "$locale has no baseListing - left unchanged"
        continue
    }
    $base = $baseProp.Value

    $kept = @()

    # Whatever is already there goes. PendingDelete rather than dropping the
    # entry: the Store removes an image when it is told to, and an image simply
    # missing from the array is left alone. Their `id` has to survive, because
    # that is what identifies the one to remove.
    $existingProp = Get-Prop $base 'images'
    if ($existingProp -and $existingProp.Value) {
        foreach ($old in @($existingProp.Value)) {
            $statusProp = Get-Prop $old 'fileStatus'
            if ($statusProp -and $statusProp.Value -ieq 'PendingUpload') {
                # Staged by an earlier run of this script and never committed.
                # Dropping it is right: its bytes are about to be replaced.
                continue
            }
            if ($statusProp) {
                $statusProp.Value = 'PendingDelete'
            } else {
                $old | Add-Member -NotePropertyName 'fileStatus' -NotePropertyValue 'PendingDelete'
            }
            $kept += $old
        }
    }

    foreach ($image in $images) {
        $kept += [pscustomobject]@{
            fileName    = $image.ZipName
            fileStatus  = 'PendingUpload'
            # "Screenshot" is the desktop one. The Mobile/Xbox/HoloLens types
            # are for those device families and would be rejected here.
            imageType   = 'Screenshot'
            description = ''
        }
    }

    if ($existingProp) {
        $existingProp.Value = $kept
    } else {
        $base | Add-Member -NotePropertyName 'images' -NotePropertyValue $kept
    }
    $updated += $locale
}

if ($updated.Count -eq 0) {
    throw "no listing was updated - nothing would change, so this is a mistake not a no-op"
}

# -Depth well past the submission's nesting: the default of 2 silently turns
# everything deeper into strings, which uploads cleanly and destroys the listing.
$json = $submission | ConvertTo-Json -Depth 100 -Compress

# WriteAllText with an explicit no-BOM encoder rather than Set-Content: 5.1's
# `-Encoding utf8` writes a BOM, and a BOM in front of the JSON is not JSON as
# far as the parser on the other end is concerned.
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
$outFull = [System.IO.Path]::GetFullPath(
    [System.IO.Path]::Combine((Get-Location).Path, $OutPath))
[System.IO.File]::WriteAllText($outFull, $json, $utf8NoBom)

Write-Host "set $($images.Count) screenshot(s) on $($updated.Count) language(s)"
Write-Host "wrote $OutPath"

# ---------------------------------------------------------------------------
# The archive
# ---------------------------------------------------------------------------

if (-not $ZipPath) {
    Write-Host "no -ZipPath given, so the images were not packed - the JSON alone will not commit"
    return
}

Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

$zipFull = [System.IO.Path]::GetFullPath(
    [System.IO.Path]::Combine((Get-Location).Path, $ZipPath))
if (Test-Path $zipFull) { Remove-Item -LiteralPath $zipFull -Force }

$zip = [System.IO.Compression.ZipFile]::Open($zipFull, 'Create')
try {
    foreach ($image in $images) {
        [void][System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
            $zip, $image.Path, $image.ZipName)
    }
} finally {
    $zip.Dispose()
}
Write-Host "wrote $ZipPath"

if (-not $Upload) {
    return
}

# ---------------------------------------------------------------------------
# Merge into the submission's own archive
# ---------------------------------------------------------------------------
#
# `msstore publish --noCommit` has already uploaded an archive containing the
# package. Replacing it would lose the package, so this reads it back, adds the
# images and puts the whole thing back. The SAS the API hands out carries read
# and write, which is what makes that possible.

$urlProp = Get-Prop $submission 'fileUploadUrl'
if (-not $urlProp -or -not $urlProp.Value) {
    throw @"
the submission has no fileUploadUrl, so the images cannot be uploaded.

That URL is what the archive is written to, and only a live draft has one - a
dump taken from an already-published submission will not. Take the dump again
straight after ``msstore publish --noCommit`` and rerun with -Upload.
"@
}
$uploadUrl = $urlProp.Value

$existingZip = Join-Path ([System.IO.Path]::GetTempPath()) "store-existing.zip"
if (Test-Path $existingZip) { Remove-Item -LiteralPath $existingZip -Force }

Write-Host "reading the archive already uploaded for this submission..."
try {
    Invoke-WebRequest -Uri $uploadUrl -OutFile $existingZip -UseBasicParsing
} catch {
    throw "could not read the submission's archive: $($_.Exception.Message)"
}

$merged = [System.IO.Compression.ZipFile]::Open($existingZip, 'Update')
try {
    foreach ($image in $images) {
        # Replace rather than duplicate: a second entry with the same name is
        # legal in a ZIP and reads back as whichever the reader finds first.
        $clash = $merged.Entries | Where-Object { $_.FullName -eq $image.ZipName }
        foreach ($entry in @($clash)) { $entry.Delete() }
        [void][System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
            $merged, $image.Path, $image.ZipName)
    }
} finally {
    $merged.Dispose()
}

Write-Host "putting the merged archive back..."
$headers = @{ 'x-ms-blob-type' = 'BlockBlob' }
try {
    Invoke-WebRequest -Uri $uploadUrl -Method Put -Headers $headers `
        -InFile $existingZip -ContentType 'application/zip' -UseBasicParsing | Out-Null
} catch {
    throw "could not upload the merged archive: $($_.Exception.Message)"
}

Write-Host "uploaded: package and $($images.Count) screenshot(s) now travel together"
