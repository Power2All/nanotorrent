# Microsoft Store listing text

One file per language, holding every text field Partner Center asks for in a
Store listing. Paste each block into the matching field under
**Store listings ▸ _language_**.

There are 41 files, one for every locale in `lang/` — the same set the MSIX
manifest declares as supported languages.

Plus `restricted-capability-justification.txt`, which is not a listing: it is
the `runFullTrust` justification Partner Center asks for on every submission,
the same text each time, and it carries no version.

## You almost certainly do not need all 41

Partner Center only asks for a listing in languages **you add to the
submission**. Adding a language and leaving its fields empty blocks the
submission, so add only the ones you actually intend to publish.

Start with `en-US` alone. It is the fallback every market falls back to, and an
English-only listing still ships to every country you have selected — listing
language is not market availability. Languages can be added in any later
submission at no cost, so this is not a decision you are stuck with.

The other 40 are here so that adding one later is a paste rather than a
translation job.

## Field limits

| Field | Limit |
| --- | --- |
| Description | 10,000 characters |
| What's new in this version | 1,500 characters |
| Product features | up to 20 items, 200 characters each |
| Search terms | up to 7 terms, 30 characters each |
| Copyright and trademark info | 200 characters |
| Additional license terms | 10,000 characters |
| Developed by | 255 characters |

Every file was checked against these when generated. Nothing is over.

## File naming

Files are named by the **Partner Center** language code, which differs from the
app's locale code in three cases:

| App locale (`lang/`) | This folder | Partner Center calls it |
| --- | --- | --- |
| `zh-CN` | `zh-Hans.txt` | Chinese (Simplified) |
| `zh-TW` | `zh-Hant.txt` | Chinese (Traditional) |
| `sr-SP` | `sr-Cyrl.txt` | Serbian (Cyrillic, Serbia) |

Each file's header names both.

## Translation quality

`en-US.txt` is the original. The other 40 are machine translations, consistent
with how the app's own `lang/*.json` are produced and with the note in
AI-DECLARATION.md.

Store listings are read by human reviewers, unlike UI strings, so have a native
speaker glance over any language before you publish it. This matters more for
the Description than for the feature bullets.

## What the copy claims

The feature claims were checked against the BEP support table in the README and
the actual code, not written from marketing habit.

**The Description now under-claims.** It was written when the engine had no uTP,
no local service discovery and no web seeds; all three work as of 0.3.5, along
with HTTP seeding (BEP 17) and v2 seeding. The copy has not been rewritten to
say so, because doing that means retranslating a paragraph in 41 files - so if
you want the listing to claim them, the affected line is the "DHT, peer
exchange, tracker tiers and UDP trackers" feature bullet and the corresponding
sentence in the Description, and both need a pass in every language.

Nothing currently claimed is false. The gap is the other way round.

## Also needed for the submission, and not in this folder

- **Screenshots** — `images/*.png` in the repository root. All seven meet the
  1366×768 minimum, and the release workflow now pushes them to **every**
  language on each submission, so they no longer have to be attached by hand
  and cannot drift out of date per language. See `installer/store-images.ps1`
  and the "Screenshots" section of `docs/MICROSOFT-STORE.md`.
- **Privacy policy URL** — `privacy.txt` in the repository root has the text; it
  must be reachable at a URL, and the file names
  `https://www.nanotorrent.org/privacy.txt`.
- **Package identity** — see `docs/MICROSOFT-STORE.md`.

## Regenerating

These were produced by scripts that no longer live in the repository. To change
wording across all languages, edit the files directly — there are only 41, and
each is self-contained.

The "What's new" block is the one field that must change every release. It is
the second block in each file, and the version number appears twice: in the
banner at the top and on the first line of that block. The word for "Version"
is translated in each file; only the number changes.

Screenshots are **not** in these files. They come from `images/*.png` and are
the same seven for every language, which is why they are pushed by a script
rather than pasted per listing. If a language ever needs its own screenshots -
a localised window, say - that is the point at which this folder would grow an
image list, and `store-images.ps1` would read it instead of the folder.
