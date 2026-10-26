#!/usr/bin/env python3
"""RB-6 scenario HTTP server: serves the bundle directory like `python3 -m http.server` (one thread per
request), plus bad downloads for install_kernel's fetch paths:
  /cutbody/<file>  Content-Length of the good file, half the body, then the connection closes
  /huge/<file>     Content-Length 4 GiB (over every limit), no body
  /slow/<file>     the good file after a pause of SLOW_S seconds (keeps an install busy for the busy refusal)
Anything not in the directory answers 404.

Usage: install_http.py <port> <bundle dir>"""
import http.server
import os
import sys
import time

PORT, ROOT = int(sys.argv[1]), sys.argv[2]
SLOW_S = 8


class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **kw):
        super().__init__(*a, directory=ROOT, **kw)

    def good(self, name):
        p = os.path.join(ROOT, "good", os.path.basename(name))
        with open(p, "rb") as f:
            return f.read()

    def head(self, length):
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(length))
        self.send_header("Connection", "close")
        self.end_headers()

    def do_GET(self):
        parts = self.path.split("?")[0].strip("/").split("/")
        if len(parts) == 2 and parts[0] in ("cutbody", "huge", "slow") and parts[1] in ("KERNEL", "INITRD"):
            kind, name = parts
            self.close_connection = True
            if kind == "huge":
                self.head(4 << 30)
                return
            data = self.good(name)
            if kind == "slow":
                time.sleep(SLOW_S)
                self.head(len(data))
                self.wfile.write(data)
                return
            self.head(len(data))
            self.wfile.write(data[: len(data) // 2])
            self.wfile.flush()
            return
        super().do_GET()

    def log_message(self, fmt, *args):
        sys.stderr.write("%s %s\n" % (self.address_string(), fmt % args))


http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
