# Web remote

The optional HTTP interface (`src/webui/index.html`), rendered by Edge 151
headless against a live headless NanoTorrent with three synthetic torrents in
three different states.

Captured with no screen interaction at all:

    msedge --headless=new --disable-gpu --hide-scrollbars \
           --user-data-dir=<fresh dir> --window-size=1280,420 \
           --virtual-time-budget=10000 \
           --screenshot=out.png "http://127.0.0.1:8444/?refresh=0&theme=dark"

Three things that invocation depends on:

- **`?refresh=0`.** `--virtual-time-budget` captures once the network falls
  idle, and a page polling every two seconds never does - the run hangs until
  killed.
- **A fresh `--user-data-dir` per run.** A profile left locked by a killed Edge
  makes the next run exit silently without writing a file.
- **The auth proxy.** Headless Chromium has no credential prompt and does not
  carry URL-embedded credentials into `fetch()`, so a screenshot needs the
  Authorization header injected in front of the server. See
  `tools/api/README.md`; nothing about the server changes for it.

| File | View |
|---|---|
| `01-dark.png` | Dark theme, desktop width |
| `02-light.png` | Light theme, desktop width |
| `03-mobile.png` | Dark theme at 420px, the phone case |

Not captured, because they need interaction rather than a page load: the Add
torrent dialog and the server-side folder browser.
