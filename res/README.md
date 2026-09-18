# Artwork

Two PNGs and an icon, and which one to use depends on where it ends up.

| File | Size | Used by |
|---|---|---|
| `app.png` | 2048×2048 | The **master**. `installer/build-msix.ps1` scales it into every Store logo, `installer/make-assets.ps1` draws the NSIS welcome image from it, and the macOS job downscales it into an `.icns` with `sips`. Never referenced at runtime, and never installed as-is - see below. |
| `app-256.png` | 256×256 | Everything that ships **inside** the binary - the Slint window icons, the toast icon, the web interface's favicon, the README - and every Linux install: deb, rpm, the AppImage and `install-desktop-entry.sh`. |
| `about-logo.png` | 256×256 | The logo drawn **inside** the About box, and nowhere else. Byte-for-byte what `app-256.png` looks like, and deliberately not the same file - see below. |
| `app.ico` | 10 sizes | Embedded as a Win32 resource by `build.rs`, so Explorer, the taskbar and the NSIS installer get it. |

`app-256.png` is generated from `app.png` and should be regenerated whenever
that changes:

```powershell
Add-Type -AssemblyName System.Drawing
$src = [System.Drawing.Image]::FromFile("res\app.png")
$bmp = New-Object System.Drawing.Bitmap 256, 256
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
$g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
$g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
$g.Clear([System.Drawing.Color]::Transparent)
$g.DrawImage($src, 0, 0, 256, 256)
$bmp.Save("res\app-256.png", [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose(); $src.Dispose()
```

Why two rather than one: the master is about 2 MB, and it was being
`include_bytes!`'d into the binary twice — once for notifications, once as the
favicon the web interface serves with a day's cache lifetime — as well as
decoded by Slint for each of a dozen window icons. A window icon is never
larger than 256px and a favicon is 32. The Store is the one consumer that
genuinely wants the resolution, and it does its own scaling at package time, so
it keeps the master.

Why `about-logo.png` exists as a separate file, when it is the same picture
at the same size as `app-256.png`: the About window binds `app-256.png` to its
`icon` property, and a Slint window may not also *draw* the file its own icon
comes from. If it does, every window shown afterwards loses its icon and gets
the generic Windows one instead - open About, close it, open it again, and the
second About has no icon, nor does Preferences after it. Shipped that way in
v0.4.0. Regenerate it alongside `app-256.png`, with the same recipe and size:

```python
from PIL import Image
m = Image.open("res/app.png").convert("RGBA")
m.resize((256, 256), Image.LANCZOS).save("res/about-logo.png")
```

`app.ico` is likewise generated from `app.png`, and carries 16, 20, 24, 32, 40,
48, 64, 96, 128 and 256. The odd sizes are not padding: Windows 11's taskbar
asks for 24 at 100% DPI and 36 at 150%, and Explorer's Large-icons view asks
for 96. An icon without them still draws — Windows downscales the next size up
at paint time — but softly, which is what a "the icon looks wrong" report
usually turns out to be. Entries are PNG-compressed, which `makensis` accepts
and which costs 99 KB against 370 KB for the all-BMP equivalent.

Save as a `.py` and run it — `System.Drawing` is no use here, it can read a
multi-size icon but not author one. Needs Pillow (`pip install pillow`):

```python
from PIL import Image
m = Image.open("res/app.png").convert("RGBA")
s = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]
f = [m.resize((n, n), Image.LANCZOS) for n in s]
f[-1].save("res/app.ico", format="ICO",
           sizes=[(n, n) for n in s], append_images=f[:-1])
```

After regenerating either file, rebuild: `build.rs` has
`rerun-if-changed=res/app.ico`, but a taskbar icon that still looks stale is
more often a NanoTorrent already running (it is single-instance, so a new
launch hands off to the old process and you are looking at *its* window) than
a build that missed the change.

Linux takes the 256 derivative, never the master. The icon lands in
`hicolor/256x256/`, and a file in that directory has to *be* 256x256;
`linuxdeploy` goes further and refuses any icon whose size is not one of the
standard ones, so pointing the AppImage at a 2048px master fails the build
outright rather than merely looking wrong. That is not hypothetical - the
master was 256x256 until it was replaced with a 2048px one, and the packaging
still pointed at it. `every_linux_consumer_installs_the_256_icon` in
`src/core/toast.rs` now fails if any of the four consumers drifts back.

`flags/` is the country artwork for the peers list, 32×24 each, embedded by a
table `build.rs` generates.
