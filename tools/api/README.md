# Web API smoke test

`smoke.py` exercises the running web interface end to end - the parts unit
tests cannot reach, because they are about HTTP status codes, authentication
and real filesystem effects rather than pure functions.

    cargo build --no-default-features          # headless: opens no window
    mkdir /tmp/nt && cp target/debug/nanotorrent.exe /tmp/nt/ && touch /tmp/nt/portable.txt
    /tmp/nt/nanotorrent.exe --webui on
    printf 'baseline-test-password' | /tmp/nt/nanotorrent.exe --set-web-password
    /tmp/nt/nanotorrent.exe &                  # leave it running
    python tools/api/smoke.py /tmp/nt

`portable.txt` keeps the whole instance inside that folder, so a test run never
touches a real NanoTorrent profile.

It expects the credentials above and `https://127.0.0.1:8443`, and skips
certificate verification because the server generates its own. Both are
hardcoded: this is a local smoke test, not a deployment tool.

What it covers that the unit tests do not:

- authentication on every route, including the ones added last;
- 404 rather than a silent no-op for an unknown info hash;
- `mkdir` refusing to clobber or to create intermediates, verified on disk;
- the canonical path contract - what a client is told it browsed is what was
  actually read (8.3 short names expanded, separators normalised);
- `magnet:` being the only accepted URL scheme, so "add a torrent" cannot be
  turned into "make the server fetch this URL".

## Screenshotting the page (authproxy.py)

Headless Chromium has no Basic-auth prompt and does not carry URL-embedded
credentials into `fetch()`, so the page loads but every API call 401s and the
result is an empty table. `authproxy.py` sits in front of the server on plain
HTTP and adds the Authorization header:

    python tools/api/authproxy.py 8444          # forwards to https://127.0.0.1:8443

Then point a headless browser at `http://127.0.0.1:8444/?refresh=0`. Nothing
about the server changes - the proxy speaks to it exactly as a browser would,
and it is loopback-only test scaffolding. See `docs/web-remote/README.md` for
the full browser invocation and the two other things it depends on.
