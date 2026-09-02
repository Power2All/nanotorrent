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
installeruild-msix.ps1 -NoSign
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

## Automating the update from a GitHub release

`.github/workflows/store-publish.yml` builds the MSIX and publishes it whenever
a release is published, using the
[Microsoft Store Developer CLI](https://learn.microsoft.com/windows/apps/publish/msstore-dev-cli/github-actions)
(`microsoft/microsoft-store-apppublisher@v1.1`).

It slots into the existing flow rather than replacing it: `release.yml` drafts a
release, you press **Publish**, and that fires this workflow. The distinction
matters - a release created by a workflow using `GITHUB_TOKEN` does *not*
trigger other workflows, so an automated publish would have gone unnoticed. It
is the human pressing Publish that starts this.

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
