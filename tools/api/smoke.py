"""End-to-end check of the NanoTorrent web API against a running headless instance."""
import base64, json, os, ssl, sys, time, urllib.request, urllib.error

BASE = "https://127.0.0.1:8443"
AUTH = base64.b64encode(b"nanotorrent:baseline-test-password").decode()
CTX  = ssl._create_unverified_context()          # self-signed by design
ROOT = sys.argv[1]                                # the apitest folder

ok = fail = 0
def check(label, got, want):
    global ok, fail
    if got == want:
        ok += 1;   print(f"  PASS  {label:52} {got}")
    else:
        fail += 1; print(f"  FAIL  {label:52} got {got}, want {want}")

def call(method, path, body=None, auth=True):
    req = urllib.request.Request(BASE + path, method=method)
    if auth: req.add_header("Authorization", "Basic " + AUTH)
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, data, context=CTX, timeout=20) as r:
            raw = r.read()
            return r.status, (json.loads(raw) if raw else None)
    except urllib.error.HTTPError as e:
        return e.code, None

print("--- filesystem ---")
newdir = os.path.join(ROOT, "made-by-api")
st, listing = call("POST", "/api/fs/mkdir", {"path": newdir})
check("mkdir new directory", st, 201)
check("directory exists on disk", os.path.isdir(newdir), True)
if listing: check("mkdir returns the CANONICAL path", listing["path"].lower(), os.path.realpath(newdir).lower())
check("mkdir refuses to clobber", call("POST", "/api/fs/mkdir", {"path": newdir})[0], 400)
ghost = os.path.join(ROOT, "ghost", "deeper")
check("mkdir refuses missing intermediate", call("POST", "/api/fs/mkdir", {"path": ghost})[0], 400)
check("  ...and created no tree", os.path.exists(os.path.join(ROOT, "ghost")), False)
check("mkdir rejects relative", call("POST", "/api/fs/mkdir", {"path": "rel/x"})[0], 400)

print("--- torrents: add ---")
tor = os.path.join(ROOT, "..", "uibase", "torrents", "Sample-Video-1080p.mkv.torrent")
blob = base64.b64encode(open(tor, "rb").read()).decode()
check("add rejects both sources at once",
      call("POST", "/api/torrents", {"magnet": "magnet:?x", "torrent_file": blob})[0], 400)
check("add rejects neither source", call("POST", "/api/torrents", {})[0], 400)
check("add rejects http:// as a magnet",
      call("POST", "/api/torrents", {"magnet": "http://evil.invalid/x.torrent"})[0], 400)
check("add rejects bad base64", call("POST", "/api/torrents", {"torrent_file": "!!!"})[0], 400)
st, _ = call("POST", "/api/torrents", {"torrent_file": blob, "save_path": newdir, "start": False})
check("add a .torrent (async, so 202)", st, 202)

hashes = []
for _ in range(20):
    time.sleep(1)
    _, rows = call("GET", "/api/torrents")
    hashes = [r["info_hash"] for r in (rows or [])]
    if hashes: break
check("torrent appears in the list", len(hashes) > 0, True)

if hashes:
    h = hashes[0]
    print("--- torrents: mutate ---")
    check("pause",   call("POST", f"/api/torrents/{h}/pause")[0], 204)
    check("resume",  call("POST", f"/api/torrents/{h}/resume")[0], 204)
    check("recheck", call("POST", f"/api/torrents/{h}/recheck")[0], 204)
    check("label",   call("POST", f"/api/torrents/{h}/label", {"label_id": None})[0], 204)
    check("move rejects relative path",
          call("POST", f"/api/torrents/{h}/move", {"path": "not/absolute"})[0], 400)
    bogus = "0" * 40
    check("unknown hash -> 404", call("POST", f"/api/torrents/{bogus}/pause")[0], 404)
    check("delete unknown hash -> 404", call("DELETE", f"/api/torrents/{bogus}")[0], 404)
    check("remove (keeps files)", call("DELETE", f"/api/torrents/{h}")[0], 204)

print("--- auth still covers everything new ---")
for m, p in [("GET", "/api/fs/roots"), ("POST", "/api/fs/mkdir"), ("POST", "/api/torrents"),
             ("DELETE", "/api/torrents/x"), ("GET", "/api/errors")]:
    check(f"{m} {p} unauthenticated", call(m, p, {} if m == "POST" else None, auth=False)[0], 401)

print("--- errors channel ---")
st, body = call("GET", "/api/errors")
check("errors endpoint responds", st, 200)
check("errors is a list", isinstance((body or {}).get("errors"), list), True)

print(f"\n{ok} passed, {fail} failed")
sys.exit(1 if fail else 0)
