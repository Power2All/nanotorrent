# Country flag icons

252 flags, one PNG per ISO 3166-1 alpha-2 code, 32x24 px.

Source: **flagpedia.net** (`https://flagcdn.com/32x24/<code>.png`).

The flag images are in the **public domain** - flags of countries are not
copyrightable - so no attribution is required and they can be redistributed
inside the NanoTorrent binary. flagpedia states this explicitly:
<https://flagpedia.net/download/api>.

They are 32x24 rather than the 16x12 a list view needs, so a high-DPI display
has real pixels to scale from; `flags.rs` downsamples to the DPI-correct size
at runtime.

Re-download with:

    powershell -File tools\update-flags.ps1
