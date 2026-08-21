# Web remote

The optional HTTP interface (`src/webui/index.html`), rendered by Edge 151
headless against a live headless NanoTorrent with three synthetic torrents in
three different states. No screen interaction is involved - headless opens no
window, so these can be regenerated while someone else is using the machine.

| File | View |
|---|---|
| `01-dark.png` | Dark theme, desktop width |
| `02-light.png` | Light theme, desktop width |
| `03-mobile.png` | Dark theme at 420px, the phone case |
| `04-add-dialog.png` | Add torrent dialog |
| `05-folder-browser.png` | Server-side folder browser |

## Regenerating

Page loads need nothing but a browser flag:

    msedge --headless=new --disable-gpu --hide-scrollbars \
           --user-data-dir=<fresh dir> --window-size=1180,340 \
           --virtual-time-budget=10000 \
           --screenshot=out.png "http://127.0.0.1:8444/?refresh=0&theme=dark"

Anything needing a click goes through `tools/api/uishot.py`, which drives the
page over the DevTools protocol - navigate, wait for real data, run a snippet,
capture:

    python tools/api/uishot.py "http://127.0.0.1:8444/?refresh=0" out.png \
      "(async()=>{document.getElementById('btn-add').click();
                  await new Promise(r=>setTimeout(r,400));})()" 900x600

Both depend on three things that are easy to get wrong and produce confusing
symptoms rather than errors:

- **`?refresh=0`.** `--virtual-time-budget` captures once the network falls
  idle, and a page polling every two seconds never does - the run hangs until
  killed.
- **A fresh `--user-data-dir` per run.** A profile left locked by a killed
  browser makes the next run exit silently without writing a file.
- **The auth proxy** (`tools/api/authproxy.py`). Headless Chromium has no
  credential prompt and does not carry URL-embedded credentials into `fetch()`,
  so without it the page loads and every API call 401s - you get a perfectly
  rendered empty table and no error anywhere.

Snippets passed to `uishot.py` should `await` their own work and return
something describing the page; the return value is printed. Waiting a fixed
period instead is how the folder browser was first captured empty, which looked
exactly like a bug and was not one - two sequential fetches had simply not
finished.

## Keep real data out of these

`05-folder-browser.png` browses the machine's own filesystem, so it will happily
show whatever is in `~/Downloads`. It is pointed at an isolated test folder
containing only synthetic directories. Check any regenerated shot before
committing it.
