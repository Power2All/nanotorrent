# Publishing to the Microsoft Store

Two routes are open, and the choice is mostly about a certificate.

| | MSIX | EXE (the existing NSIS installer) |
| --- | --- | --- |
| Code-signing certificate | **Not needed** — Microsoft signs the package | **Required**, chaining to a Microsoft Trusted Root CA |
| Packaging work | Manifest, logo assets, identity from Partner Center | None, the installer already qualifies |
| Who hosts the binary | Microsoft | **You** — the Store links to a versioned URL of yours |
| Install location | Package container, read-only | Wherever the installer puts it |
| File associations | Declared in the manifest | Registry, as now |

A code-signing certificate is a recurring cost, so **MSIX is the cheaper route**
unless you already have one. `installer/build-msix.ps1` builds it.

There is no PWA route. The web interface is a *remote control* for a running
desktop process, not the application — packaged on its own it would install a
page that cannot do anything until the real client is running elsewhere.

## The order of operations

The identity values come from **reserving the name**, not from submitting. You
do not need a submission - or even a package - to get them.

1. **Register** as a Windows app developer at
   [Partner Center](https://partner.microsoft.com/dashboard) (one-off fee).
2. **Reserve the name.** Apps and games > New product > **MSIX or PWA app** >
   type `NanoTorrent` > Check availability > **Reserve product name**.
3. **Read the identity off the Product identity page.** Microsoft assigns the
   `Publisher` (`CN=...`) and derives `Name` and the Package Family Name from
   the reservation. They exist from this moment on - no package needed.
   > These are **permanent**: the package identity cannot be changed once the
   > app is published, so reserve the name you intend to keep.
4. **Build** with those values (below).
5. **Submit the first version by hand** - upload the package, write the listing,
   submit for certification (up to about three business days).
6. **Automate from then on.** Once the app is live, publishing a GitHub release
   updates it. The API can only update an existing listing, never create one,
   which is the only reason step 5 is manual.

## Building the MSIX

```powershell
# A local test build: self-signed, installable for checking it runs packaged.
installer\build-msix.ps1

# What you upload. -NoSign matters: Microsoft signs the package, and a
# self-signed one is rejected.
installer\build-msix.ps1 -NoSign `
    -IdentityName        "12345Publisher.NanoTorrent" `
    -Publisher           "CN=ABCD1234-...." `
    -PublisherDisplayName "Power2All"
```

The three identity values come from **Partner Center ▸ your product ▸ Product
identity** after the name is reserved. They are not free choices: a package
whose `Identity` does not match the reservation is rejected at upload.

The identity can also come from the environment, so the real values live in one
place rather than on every command line - the same names CI uses:

```powershell
$env:STORE_IDENTITY_NAME = "12345Publisher.NanoTorrent"
$env:STORE_PUBLISHER     = "CN=ABCD1234-...."
installer\build-msix.ps1 -NoSign
```

The build prints which identity it used, and says so plainly when it was the
placeholder - a test-identity package is indistinguishable otherwise, and is
rejected only at upload.

The script builds both binaries, generates 19 logo assets from `res/app.png`,
substitutes the manifest and calls `makeappx`. It needs the Windows 10/11 SDK
for `makeappx.exe` and `signtool.exe`, and finds the newest one itself.

To sideload the self-signed build for testing, the certificate has to be trusted
first: export it from `Cert:\CurrentUser\My` and import into **Local Machine ▸
Trusted People**, then `Add-AppxPackage`.

## What changes when the app is packaged

The MSIX runs the ordinary desktop build unchanged (`runFullTrust`), but the
container changes things. None of them crash — every affected path is
best-effort and writes to HKCU — but they behave differently:

1. **Settings move.** `%LOCALAPPDATA%\NanoTorrent` is redirected into the
   package's own store, so a Store install does not see the data of an installer
   install, and the one-time PicoTorrent import looks in the redirected path.
2. **Preferences ▸ "Set as default for .torrent files & magnet links" does
   nothing.** Its registry writes are virtualised into the package hive where
   the shell cannot see them. The associations still work — the manifest
   declares them — so the button is redundant rather than broken.
3. **Toasts may not appear.** `core::toast` registers an AppUserModelID and a
   Start Menu shortcut by hand; a packaged app gets its identity from the
   manifest instead, and the hand-made one does not match.
4. **The update prompt points at the Store**, not at the GitHub release page.
   This one is deliberate: the NSIS installer cannot upgrade an MSIX package,
   only install a second NanoTorrent beside it, so a packaged build is sent to
   `ms-windows-store://pdp/?PFN=<its own family name>` instead. See
   `updatechecker::download_url`. The installer asks before installing over a
   Store copy, which covers the same mistake made by hand.

Detecting a packaged process is `core::environment::package_family_name`:
`GetCurrentPackageFamilyName` answers `APPMODEL_ERROR_NO_PACKAGE` when there is
no package identity, and the family name itself is what item 4 needs. Fixing 2
and 3 is now a branch on that rather than new machinery - skip the associations
writes and skip the hand-made shortcut when it returns `Some`. Neither is done,
and neither can be checked from an unpackaged dev build, which is the reason to
do them together with a real Store install in front of you.

## The same thing from CI, if you ever want it

`.github/workflows/store-publish.yml` does what `store-submit.ps1` does, using
the
[Microsoft Store Developer CLI](https://learn.microsoft.com/windows/apps/publish/msstore-dev-cli/github-actions)
(`microsoft/microsoft-store-apppublisher@v1.1`).

**It is manual-only and does not run on a published release.** It used to, and
that was turned off: submissions are made from a developer machine, and a
workflow firing on Publish would put a second submission on the same product
behind the first. Running it is now a deliberate act - Actions ▸ Publish to
Microsoft Store ▸ Run workflow.

If you ever switch back to the CI route, restore the trigger with:

```yaml
on:
  release:
    types: [published]
  workflow_dispatch:
```

and stop using the local script, rather than running both. Worth knowing if you
do: a release created by a workflow using `GITHUB_TOKEN` does *not* trigger
other workflows, so it is the human pressing **Publish** that would start it.

The package is kept as a **build artifact** (30 days), not attached to the
release: `release.yml` deliberately refuses to alter a published release's
assets, and this does not do it by the back door either. If you would rather
ship the MSIX as a downloadable asset, build it in `release.yml`'s Windows job
so it lands in the draft before anything is published.

Two limits decide whether this is usable at all:

- **The first submission must be made by hand.** The API updates an app that is
  already published and live; it cannot create the listing.
- **Microsoft documents this as supported for free products only.** A paid
  listing has to be updated manually.

Required repository secrets:

| Secret | Where it comes from |
| --- | --- |
| `AZURE_AD_TENANT_ID` | Entra ▸ Identity ▸ Overview |
| `AZURE_AD_APPLICATION_CLIENT_ID` | Entra ▸ App registrations ▸ your app |
| `AZURE_AD_APPLICATION_SECRET` | that app ▸ Certificates & secrets |
| `SELLER_ID` | Partner Center ▸ Account settings ▸ Legal info ▸ Developer ▸ Publisher IDs |
| `STORE_PRODUCT_ID` | Partner Center ▸ Product identity ▸ Store ID (12 chars, starts `9`) |
| `STORE_IDENTITY_NAME` | Partner Center ▸ Product identity ▸ Name |
| `STORE_PUBLISHER` | Partner Center ▸ Product identity ▸ Publisher (`CN=…`) |

Seller ID and Publisher ID sit next to each other and are not the same: the
Seller ID is the short numeric one, the Publisher ID is the `CN=`-style GUID.
`msstore reconfigure --sellerId` wants the numeric one.

### From your own machine

> **This is the route.** The GitHub workflow no longer runs on a published
> release - it is manual-only, kept as a fallback. Submitting from two places
> at once would put two submissions on the same product, so pick one, and the
> one that is picked is this.

The same thing without GitHub. One-time setup:

```
winget install Microsoft.DotNet.DesktopRuntime.9
winget install "Microsoft Store Developer CLI"
msstore                      # first run walks through signing in
```

Sign in with the **Microsoft Entra ID** account associated with the Partner
Center account. The CLI rejects a personal Microsoft account, which is the
first thing that catches people out.

Then one command - though it needs three values, not one:

```powershell
$env:STORE_IDENTITY_NAME = '12345Publisher.NanoTorrent'   # Product identity > Name
$env:STORE_PUBLISHER     = 'CN=<guid>'                    # Product identity > Publisher
.\installer\store-submit.ps1 -ProductId 9NBLGGH4XXXX      # Product identity > Store ID
```

All three sit on the same Partner Center page. The Store ID identifies the
product to the API; the other two go *inside* the package, and Partner Center
rejects a package whose identity does not match the reservation. Set them once
in your profile and the command really is just the Store ID after that; they
can also be passed as `-IdentityName` and `-Publisher`.

The script refuses to build without them rather than warning and carrying on,
because the failure would otherwise arrive at upload, after a full release
build. `-Msix <path>` does not ask for them - that package already has an
identity, whatever it is.

It builds the MSIX, uploads it as a draft, rewrites "What's new" for every
language, and asks before committing - a commit goes to certification and is
public once it passes, so it is not a thing to do by accident. Answer no and
the draft is left in Partner Center to review by hand; `-Yes` skips the
question for an unattended run.

Useful variations:

| | |
| --- | --- |
| `-DryRun` | Print every step, run none of them |
| `-Msix <path>` | Submit a package you already built |
| `-Yes` | Do not ask before committing |

The Store ID can live in `STORE_PRODUCT_ID` instead of being retyped, and
`build-msix.ps1` already reads `STORE_IDENTITY_NAME` and `STORE_PUBLISHER` from
the environment the same way.

Nothing about this needs the GitHub secrets: the CLI keeps its own credentials
on the machine after the first sign-in. The workflow and this script run the
same sequence and share `store-whatsnew.ps1`, so a listing that submits from
one submits from the other.

### The listings go up with the package

`msstore publish` uploads the package and nothing else - it does not touch
listing metadata. That is why the "What's new" text for all 41 languages used
to be pasted into Partner Center by hand after every release.

`msstore submission update` does take listings, but only as the *whole*
submission JSON. So the workflow reads the draft, rewrites one field per
language from `MS_Store_Release_Info/*.txt`, and sends it back:

```
msstore publish <msix> -id <product> --noCommit   # package only, stays a draft
msstore submission get <product>                  # the whole submission, as JSON
installer\store-whatsnew.ps1 ...                  # rewrite releaseNotes per language
msstore submission update <product> <json>        # send it back
msstore submission publish <product>              # commit for certification
```

**The order is not the obvious one.** For an app that is already published,
`msstore publish` *deletes* the pending draft and creates a fresh one from the
last published submission. Writing the release notes first would silently throw
them away. Package first, metadata second, commit last.

`installer\store-whatsnew.ps1` can be run on its own, against a saved
`msstore submission get` dump, with no credentials and no release:

```powershell
msstore submission get <product> | Out-File -Encoding utf8 sub.json
.\installer\store-whatsnew.ps1 -SubmissionPath sub.json -OutPath sub.new.json -Version 0.3.2
```

It refuses to write anything if a listing's "What's new" does not mention the
version being shipped, which is the mistake the hand-editing invites - the
listing files carry the version in their text and are easy to forget. It also
warns about languages live in the Store with no file here, and files with no
matching Store language, leaving both untouched rather than guessing.

The Entra application also has to be added under **Partner Center ▸ Account
settings ▸ User management ▸ Microsoft Entra applications** with the **Manager**
role, or the CLI authenticates but is not allowed to submit.

> **This requires a Company developer account.** Entra work-account features are
> company-only - an Individual account sees just a *Users* tab there, with no
> *Microsoft Entra applications* tab, and so cannot create the credential the
> API needs. Account type is on Account settings ▸ Account.
>
> Individual cannot be converted to Company; it would mean a new account, a new
> name reservation and a new package identity. Not worth it for CI. On an
> Individual account, build the MSIX in CI as an artifact and upload that one
> file by hand - `build-msix.ps1` already does everything except the upload.

## If you take the EXE route instead

The existing NSIS installer already meets the Store's requirements bar one:

- **Silent install** — NSIS supports `/S` natively, and the Store needs a switch
  it can pass. ✔
- **Uninstall entries** — `DisplayName`, `DisplayVersion`, `Publisher`,
  `UninstallString`, `InstallLocation`, `NoModify`, `NoRepair` are all
  written. ✔
- **Standalone, not a downloader stub** — the payload is inside. ✔
- **Authenticode-signed**, chaining to a CA in the Microsoft Trusted Root
  Program — **not done**. This is the blocker, and the reason MSIX is cheaper.

The Store does not host an EXE submission: you give Partner Center a versioned
download URL (a GitHub release asset works), and the binary at that URL must not
change afterwards.
