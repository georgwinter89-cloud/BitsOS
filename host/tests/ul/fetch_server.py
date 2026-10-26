"""UL-4a: HTTPS and HTTP partner for fetch's cases (user/tools/fetch/tests/cases.txt).

usage: python3 -I fetch_server.py DIR HTTPS_PORT HTTP_PORT

Makes in DIR, with the openssl CLI: a test CA (ca.pem), a second unrelated CA (ca2.pem), a server certificate for
127.0.0.1, 10.0.2.2 (the host as QEMU's user network shows it) and localhost signed by the first CA; a 3 MiB random
blob and its SHA-256 (blob.sha256). Serves the same paths on both ports (TLS on the first) from 127.0.0.1, then
writes DIR/ready. Each connection is handled in its own thread, the TLS handshake included, so a stalled or
non-TLS client cannot block the others.
"""

import hashlib
import os
import ssl
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

BLOB_SIZE = 3 * 1024 * 1024
SMALL = b"hello from the fetch test server\n"


def openssl(*args):
    subprocess.run(["openssl", *args], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def make_files(d):
    for ca in ("ca", "ca2"):
        openssl("req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-days", "30",
                "-subj", f"/CN=BitsOS fetch test {ca}", "-keyout", f"{d}/{ca}.key", "-out", f"{d}/{ca}.pem",
                "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign")
    openssl("req", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-subj", "/CN=127.0.0.1",
            "-keyout", f"{d}/server.key", "-out", f"{d}/server.csr")
    with open(f"{d}/ext.cnf", "w") as f:
        f.write("basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\n"
                "extendedKeyUsage=serverAuth\nsubjectAltName=IP:127.0.0.1,IP:10.0.2.2,DNS:localhost\n")
    openssl("x509", "-req", "-in", f"{d}/server.csr", "-CA", f"{d}/ca.pem", "-CAkey", f"{d}/ca.key",
            "-CAcreateserial", "-days", "30", "-extfile", f"{d}/ext.cnf", "-out", f"{d}/server.pem")
    blob = os.urandom(BLOB_SIZE)
    with open(f"{d}/blob", "wb") as f:
        f.write(blob)
    with open(f"{d}/blob.sha256", "w") as f:
        f.write(hashlib.sha256(blob).hexdigest() + "\n")
    return blob


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        sys.stderr.write("fetch-server: " + (fmt % args) + "\n")

    def head(self, code, length=None, extra=()):
        self.send_response(code)
        if length is not None:
            self.send_header("Content-Length", str(length))
        for k, v in extra:
            self.send_header(k, v)
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True

    def do_GET(self):
        d, blob = self.server.dir, self.server.blob
        p = self.path.split("?")[0]
        files = {"/ca.pem": "ca.pem", "/ca2.pem": "ca2.pem", "/blob.sha256": "blob.sha256"}
        if p in files:
            data = open(f"{d}/{files[p]}", "rb").read()
            self.head(200, len(data))
            self.wfile.write(data)
        elif p == "/blob":
            self.head(200, len(blob))
            self.wfile.write(blob)
        elif p == "/small":
            self.head(200, len(SMALL))
            self.wfile.write(SMALL)
        elif p == "/chunked":
            self.head(200, extra=[("Transfer-Encoding", "chunked")])
            step = 100_003
            for i in range(0, len(blob), step):
                part = blob[i:i + step]
                self.wfile.write(b"%x;n=%d\r\n" % (len(part), i) + part + b"\r\n")
            self.wfile.write(b"0\r\nX-Trailer: done\r\n\r\n")
        elif p in ("/close", "/close-trunc"):
            self.head(200)
            self.wfile.write(blob)
            self.wfile.flush()
            if p == "/close" and isinstance(self.connection, ssl.SSLSocket):
                try:
                    self.connection.unwrap()  # sends close_notify: a clean TLS end
                except (OSError, ValueError):
                    pass
        elif p == "/short":
            self.head(200, 1000)
            self.wfile.write(b"x" * 10)
        elif p == "/stall":
            self.head(200, 1000)
            self.wfile.write(b"x" * 100)
            self.wfile.flush()
            time.sleep(20)
        elif p == "/nocontent":
            self.head(204)  # no length, then the connection stays open: fetch must not wait for its end
            self.wfile.flush()
            time.sleep(20)
        elif p == "/bighead":
            self.send_response(200)
            for i in range(300):  # 300 KiB of headers, over fetch's 256 KiB cap
                self.send_header(f"X-Pad-{i}", "a" * 1024)
            self.send_header("Content-Length", "0")
            self.end_headers()
            self.close_connection = True
        elif p == "/trailer-flood":
            self.head(200, extra=[("Transfer-Encoding", "chunked")])
            self.wfile.write(b"1\r\nz\r\n0\r\n")
            for i in range(4096):  # 4 MiB of trailer lines; fetch gives up at 256 KiB and closes
                self.wfile.write(b"X-T-%d: %s\r\n" % (i, b"t" * 1000))
            self.wfile.write(b"\r\n")
        elif p == "/drip":
            self.head(200, 1000)
            for _ in range(1000):  # one byte every 0.5 s: never idle for --timeout, only --max-time stops it
                self.wfile.write(b"d")
                self.wfile.flush()
                time.sleep(0.5)
        elif p == "/redir-noloc":
            self.head(302, 0)
        elif p == "/redir":
            self.head(302, 0, [("Location", "blob")])
        elif p == "/loop":
            self.head(302, 0, [("Location", "/loop?again")])
        elif p == "/to-http":
            self.head(301, 0, [("Location", f"http://127.0.0.1:{self.server.http_port}/blob")])
        else:
            self.head(404, 0)


class Server(ThreadingHTTPServer):
    daemon_threads = True
    tls = None

    def finish_request(self, request, client_address):
        if self.tls is not None:
            try:
                request.settimeout(30)
                request = self.tls.wrap_socket(request, server_side=True)
            except (OSError, ssl.SSLError):
                return
        super().finish_request(request, client_address)

    def shutdown_request(self, request):
        try:
            request.close()
        except OSError:
            pass

    def handle_error(self, request, client_address):
        pass


def main():
    d, https_port, http_port = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
    blob = make_files(d)
    tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    tls.load_cert_chain(f"{d}/server.pem", f"{d}/server.key")
    servers = []
    for port, ctx in ((https_port, tls), (http_port, None)):
        s = Server(("127.0.0.1", port), Handler)
        s.tls, s.dir, s.blob, s.http_port = ctx, d, blob, http_port
        servers.append(s)
    for s in servers:
        threading.Thread(target=s.serve_forever, daemon=True).start()
    open(f"{d}/ready", "w").close()
    while True:
        time.sleep(3600)


if __name__ == "__main__":
    main()
