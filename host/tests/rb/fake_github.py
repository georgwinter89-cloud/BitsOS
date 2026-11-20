#!/usr/bin/env python3
"""UPDATE scenario: a fake GitHub for the guest's update service, over HTTPS with the run's test CA.
  API (port A): GET /repos/o/werkstatt/releases?...      the release list (every version-<N> folder of ROOT)
                GET /repos/o/werkstatt/releases/assets/<id>  302 to the storage port (another origin)
  storage (port S): GET /blob/<id>?sig=...               the asset's bytes
The API demands `Authorization: Bearer <token>` (401 otherwise); the storage refuses any Authorization (400, as
S3 does). Every request is a line in the log: `api|storage <path> auth=<yes|no>`.

Usage: fake_github.py <api port> <storage port> <cert> <key> <token> <root: rel-<N>/ folders> <log>"""
import http.server
import json
import os
import ssl
import sys
import threading

API, STORE, CERT, KEY, TOKEN, ROOT, LOG = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5], sys.argv[6], sys.argv[7]
# The fixed files first (KERNEL is id N*1000), then every other file of the folder (UPD-4: MANIFEST2, bin.*, tree.*).
FILES = ["KERNEL", "INITRD", "BOOTX64.EFI", "WASISTNEU.TXT", "MANIFEST", "MANIFEST.SIG"]
lock = threading.Lock()


def note(line):
    with lock, open(LOG, "a") as f:
        f.write(line + "\n")


def assets():
    """(id, version, name, path) of every file of every rel-<N> folder; ids are N*1000 + index."""
    out = []
    for d in sorted(os.listdir(ROOT)):
        if not d.startswith("rel-"):
            continue
        n = int(d[4:])
        rest = sorted(f for f in os.listdir(os.path.join(ROOT, d)) if f not in FILES and not f.startswith("."))
        for i, f in enumerate(FILES + rest):
            p = os.path.join(ROOT, d, f)
            if os.path.isfile(p):
                out.append((n * 1000 + i, n, f, p))
    return out


class Base(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def send(self, code, body, extra=None):
        self.send_response(code)
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)
        self.close_connection = True

    def log_message(self, fmt, *args):
        pass


class Api(Base):
    def do_GET(self):
        auth = self.headers.get("Authorization")
        note(f"api {self.path} auth={'yes' if auth else 'no'}")
        if auth != f"Bearer {TOKEN}":
            return self.send(401, b'{"message":"Bad credentials"}')
        if self.path.startswith("/repos/o/werkstatt/releases?"):
            rel = {}
            for aid, n, f, p in assets():
                rel.setdefault(n, []).append({"id": aid, "name": f, "size": os.path.getsize(p)})
            body = [{"tag_name": f"version-{n}", "draft": False, "published_at": "2026-11-16T09:00:00Z", "assets": a} for n, a in sorted(rel.items(), reverse=True)]
            return self.send(200, json.dumps(body).encode(), {"Content-Type": "application/json"})
        pre = "/repos/o/werkstatt/releases/assets/"
        if self.path.startswith(pre):
            host = self.headers.get("Host", "10.0.2.2").split(":")[0]
            return self.send(302, b"", {"Location": f"https://{host}:{STORE}/blob/{self.path[len(pre):]}?X-Amz-Signature=fake"})
        self.send(404, b"{}")


class Store(Base):
    def do_GET(self):
        auth = self.headers.get("Authorization")
        note(f"storage {self.path} auth={'yes' if auth else 'no'}")
        if auth:
            return self.send(400, b"only one auth mechanism allowed")
        aid = self.path.split("?")[0].rsplit("/", 1)[-1]
        for i, _, _, p in assets():
            if str(i) == aid:
                with open(p, "rb") as f:
                    return self.send(200, f.read(), {"Content-Type": "application/octet-stream"})
        self.send(404, b"")


def serve(port, handler):
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(CERT, KEY)
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", port), handler)
    srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
    srv.serve_forever()


threading.Thread(target=serve, args=(STORE, Store), daemon=True).start()
serve(API, Api)
