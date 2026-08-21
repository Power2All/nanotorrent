"""Screenshot the web remote after driving it, using the Chrome DevTools Protocol.

`--screenshot` alone can only capture a page load, which leaves the dialogs -
Add torrent, and the server-side folder browser - unverified. This drives the
page instead: navigate, wait for real data, run a snippet, capture.

Everything happens inside a headless browser, so it never takes over the
screen. Edge and Chrome both work; pass --browser to choose.

    python tools/api/uishot.py URL OUT.png [JS]

The optional JS runs after the torrent list has data and before the capture,
e.g. `document.getElementById('btn-add').click()`.

The WebSocket client below is hand-rolled because CDP needs one and the
standard library has none. It handles exactly what CDP requires - masked text
frames out, unmasked frames in - and nothing else: no extensions, no
continuation frames, no ping/pong. That is enough for short JSON commands over
loopback, and far less than a dependency.
"""

import base64, json, os, secrets, socket, struct, subprocess, sys, tempfile, time
import urllib.request

BROWSERS = [
    r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files\Google\Chrome\Application\chrome.exe",
    "/usr/bin/microsoft-edge",
    "/usr/bin/google-chrome",
]


class WS:
    """The smallest WebSocket client that can carry CDP."""

    def __init__(self, url):
        _, rest = url.split("://", 1)
        hostport, path = rest.split("/", 1)
        host, port = hostport.split(":")
        self.sock = socket.create_connection((host, int(port)), timeout=20)
        key = base64.b64encode(secrets.token_bytes(16)).decode()
        self.sock.sendall(
            f"GET /{path} HTTP/1.1\r\nHost: {hostport}\r\nUpgrade: websocket\r\n"
            f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n"
            f"Sec-WebSocket-Version: 13\r\n\r\n".encode()
        )
        buf = b""
        while b"\r\n\r\n" not in buf:
            buf += self.sock.recv(4096)
        if b"101" not in buf.split(b"\r\n", 1)[0]:
            raise RuntimeError("websocket upgrade refused: " + buf[:200].decode("latin1"))
        self.rest = buf.split(b"\r\n\r\n", 1)[1]

    def _read(self, n):
        while len(self.rest) < n:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise RuntimeError("websocket closed")
            self.rest += chunk
        out, self.rest = self.rest[:n], self.rest[n:]
        return out

    def send(self, text):
        payload = text.encode()
        n = len(payload)
        header = b"\x81"                      # FIN + text opcode
        if n < 126:
            header += bytes([0x80 | n])       # 0x80 = masked, required client->server
        elif n < 65536:
            header += bytes([0x80 | 126]) + struct.pack(">H", n)
        else:
            header += bytes([0x80 | 127]) + struct.pack(">Q", n)
        mask = secrets.token_bytes(4)
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        self.sock.sendall(header + mask + masked)

    def recv(self):
        b0, b1 = self._read(2)
        n = b1 & 0x7F
        if n == 126:
            n = struct.unpack(">H", self._read(2))[0]
        elif n == 127:
            n = struct.unpack(">Q", self._read(8))[0]
        if b1 & 0x80:                          # server frames are never masked
            self._read(4)
        return self._read(n).decode("utf-8", "replace")


class CDP:
    def __init__(self, ws):
        self.ws, self.n = ws, 0

    def call(self, method, **params):
        self.n += 1
        self.ws.send(json.dumps({"id": self.n, "method": method, "params": params}))
        while True:
            msg = json.loads(self.ws.recv())
            if msg.get("id") == self.n:
                if "error" in msg:
                    raise RuntimeError(f"{method}: {msg['error']}")
                return msg.get("result", {})
            # Everything else is an event; CDP interleaves them freely.

    def eval(self, expression, await_promise=False):
        r = self.call(
            "Runtime.evaluate",
            expression=expression,
            awaitPromise=await_promise,
            returnByValue=True,
        )
        if "exceptionDetails" in r:
            raise RuntimeError("page threw: " + json.dumps(r["exceptionDetails"])[:300])
        return r.get("result", {}).get("value")


def find_browser(preferred=None):
    if preferred:
        return preferred
    for path in BROWSERS:
        if os.path.exists(path):
            return path
    raise SystemExit("no Edge or Chrome found; pass --browser PATH")


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--browser=")]
    pref = next((a.split("=", 1)[1] for a in sys.argv[1:] if a.startswith("--browser=")), None)
    if len(args) < 2:
        raise SystemExit(__doc__)
    url, out = args[0], args[1]
    js = args[2] if len(args) > 2 else None
    width, height = 1280, 800
    for a in args[3:]:
        if "x" in a:
            width, height = (int(v) for v in a.split("x", 1))

    # A fresh profile every run. One left locked by a killed browser makes the
    # next run exit silently, which reads exactly like a broken script.
    profile = tempfile.mkdtemp(prefix="nt-uishot-")
    proc = subprocess.Popen(
        [
            find_browser(pref), "--headless=new", "--disable-gpu", "--hide-scrollbars",
            "--no-first-run", "--no-default-browser-check",
            f"--user-data-dir={profile}", f"--window-size={width},{height}",
            "--remote-debugging-port=0", "about:blank",
        ],
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
    )

    # Port 0 means "pick one"; the browser writes the real port here.
    portfile = os.path.join(profile, "DevToolsActivePort")
    deadline = time.time() + 30
    while time.time() < deadline:
        if os.path.exists(portfile):
            lines = open(portfile).read().split("\n")
            if len(lines) >= 2:
                port = int(lines[0])
                break
        time.sleep(0.2)
    else:
        proc.kill()
        raise SystemExit("browser never reported a DevTools port")

    try:
        targets = []
        deadline = time.time() + 20
        while time.time() < deadline and not targets:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list", timeout=5) as r:
                targets = [t for t in json.load(r) if t.get("type") == "page"]
            if not targets:
                time.sleep(0.2)
        if not targets:
            raise SystemExit("no page target")

        cdp = CDP(WS(targets[0]["webSocketDebuggerUrl"]))
        cdp.call("Page.enable")
        cdp.call("Runtime.enable")
        cdp.call("Page.navigate", url=url)

        # Wait for real data rather than the load event - the table is filled by
        # fetch(), which resolves after load.
        cdp.eval(
            """new Promise(done => {
                 const t0 = Date.now();
                 (function check() {
                   const ready = document.querySelector('#rows tr')
                              || document.getElementById('empty')?.hidden === false;
                   if (ready || Date.now() - t0 > 10000) done(true);
                   else setTimeout(check, 100);
                 })();
               })""",
            await_promise=True,
        )

        if js:
            value = cdp.eval(js, await_promise=True)
            # Printed so the snippet can double as a probe - return something
            # describing the page state and it lands on stdout next to the file.
            if value is not None:
                print(json.dumps(value)[:2000])
            # Let a dialog finish opening and any fetch it kicks off settle.
            cdp.eval("new Promise(r => setTimeout(r, 1200))", await_promise=True)

        data = cdp.call("Page.captureScreenshot", format="png")["data"]
        with open(out, "wb") as f:
            f.write(base64.b64decode(data))
        print(f"wrote {out}")
    finally:
        proc.kill()


if __name__ == "__main__":
    main()
