"""Plain-HTTP front for the web remote, adding the Basic auth header.

Test scaffolding only. Headless Chromium has no credential prompt and does not
carry URL-embedded credentials into fetch(), so a screenshot of the real page
needs the header supplied some other way. Nothing about the server changes -
this sits in front of it and speaks to it exactly as a browser would.
"""
import base64, http.server, ssl, sys, urllib.error, urllib.request

UP   = "https://127.0.0.1:8443"
AUTH = "Basic " + base64.b64encode(b"nanotorrent:baseline-test-password").decode()
CTX  = ssl._create_unverified_context()

class Proxy(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass

    def forward(self, method):
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else None
        req = urllib.request.Request(UP + self.path, data=body, method=method)
        req.add_header("Authorization", AUTH)
        if self.headers.get("Content-Type"):
            req.add_header("Content-Type", self.headers["Content-Type"])
        try:
            with urllib.request.urlopen(req, context=CTX, timeout=20) as r:
                data, status, ctype = r.read(), r.status, r.headers.get("Content-Type", "text/plain")
        except urllib.error.HTTPError as e:
            data, status, ctype = e.read(), e.code, e.headers.get("Content-Type", "text/plain")
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        if data: self.wfile.write(data)

    def do_GET(self):    self.forward("GET")
    def do_POST(self):   self.forward("POST")
    def do_DELETE(self): self.forward("DELETE")

http.server.HTTPServer(("127.0.0.1", int(sys.argv[1])), Proxy).serve_forever()
