#!/usr/bin/env python3
"""Host partner for 30-stack.sh (NW-3), reached from the guest as 10.0.2.2:<port> (QEMU user network).

GET /ping         -> "pong\n"
GET /file         -> the test file (Content-Length)
GET /chunked      -> the same file, chunked with uneven chunk sizes
GET /file.sha256  -> "<sha256 hex> <size>\n"  (the guest checks its download against it)
GET /slice/N[.sha256]               -> the first N bytes / their hash (NW-7: nw.stack.fetch_parallel)
GET /part/N/{file,file.sha256,chunked} -> the three above over the first N bytes (abi-net fetchcheck BASE/part/N)
GET /connect-in   -> "ok\n", then connects to 127.0.0.1:<fwd> (hostfwd into the guest), sends a line, reads the
                     answer line and writes it to <run_dir>/inbound.txt
anything else     -> 404
UDP <port>+1      -> every datagram is sent back (nw.stack.udp_host)
TCP <port>+2      -> echo: every byte is sent back, each connection in its own thread (NW-5: abi-net echo)

Usage: stack_server.py <port> <file> <fwd_port> <run_dir> [bind address, default 127.0.0.1]
For the Mini-PC check the Gaming-PC binds its LAN address and CONFIG.TXT gets net_test_url=http://<it>:<port>.
"""
import hashlib
import http.server
import os
import random
import socket
import sys
import threading
import time

PORT, FILE, FWD, RUN = int(sys.argv[1]), sys.argv[2], int(sys.argv[3]), sys.argv[4]
DATA = open(FILE, "rb").read()
SHA = hashlib.sha256(DATA).hexdigest()


def log(msg):
    with open(os.path.join(RUN, "http.log"), "a") as f:
        f.write(f"{time.time():.3f} {msg}\n")


def connect_in():
    time.sleep(0.5)
    out = os.path.join(RUN, "inbound.txt")
    try:
        with socket.create_connection(("127.0.0.1", FWD), timeout=20) as s:
            s.sendall(b"hello from the host\n")
            got = b""
            while not got.endswith(b"\n"):
                b = s.recv(256)
                if not b:
                    break
                got += b
            open(out, "w").write(got.decode(errors="replace"))
            log(f"connect-in: answer {got!r}")
    except OSError as e:
        open(out, "w").write(f"error {e}\n")
        log(f"connect-in: {e}")


class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        log(fmt % args)

    def send_body(self, body, ctype="application/octet-stream"):
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        try:
            self.answer()
        except (ConnectionResetError, BrokenPipeError) as e:
            # The guest resets on purpose (its size limit test).
            log(f"{self.path}: {e}")
        self.close_connection = True

    def answer(self):
        data, sha, path = DATA, SHA, self.path
        if path.startswith("/part/"):
            # NW-7 fixer: `/part/N/` + file, file.sha256, chunked = the same three over the first N bytes, so 64
            # programs running `abi-net fetchcheck BASE/part/N` at once stay short.
            num, _, rest = path[len("/part/"):].partition("/")
            n = int(num) if num.isdigit() else -1
            if 0 <= n <= len(DATA) and rest in ("file", "file.sha256", "chunked"):
                data, path = DATA[:n], "/" + rest
                sha = hashlib.sha256(data).hexdigest()
        if path == "/ping":
            self.send_body(b"pong\n", "text/plain")
        elif path == "/file":
            self.send_body(data)
        elif path == "/file.sha256":
            self.send_body(f"{sha} {len(data)}\n".encode(), "text/plain")
        elif path == "/chunked":
            self.send_response(200)
            self.send_header("Transfer-Encoding", "chunked")
            self.send_header("Connection", "close")
            self.end_headers()
            rng, i = random.Random(7), 0
            while i < len(data):
                n = rng.choice([1, 17, 1000, 4096, 65536, 300000])
                part = data[i:i + n]
                self.wfile.write(b"%x;ext=1\r\n" % len(part) + part + b"\r\n")
                i += n
            self.wfile.write(b"0\r\nX-Trailer: done\r\n\r\n")
        elif self.path.startswith("/slice/"):
            # NW-7: the first N bytes of the file (`/slice/N`) and their hash (`/slice/N.sha256`), so many
            # parallel downloads stay short.
            tail = self.path[len("/slice/"):]
            want_hash = tail.endswith(".sha256")
            num = tail[:-len(".sha256")] if want_hash else tail
            n = int(num) if num.isdigit() else -1
            if n < 0 or n > len(DATA):
                self.send_response(404)
                self.send_header("Content-Length", "0")
                self.send_header("Connection", "close")
                self.end_headers()
            elif want_hash:
                self.send_body(f"{hashlib.sha256(DATA[:n]).hexdigest()} {n}\n".encode(), "text/plain")
            else:
                self.send_body(DATA[:n])
        elif self.path == "/connect-in":
            self.send_body(b"ok\n", "text/plain")
            threading.Thread(target=connect_in, daemon=True).start()
        else:
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.send_header("Connection", "close")
            self.end_headers()


BIND = sys.argv[5] if len(sys.argv) > 5 else "127.0.0.1"


def udp_echo():
    u = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    u.bind((BIND, PORT + 1))
    while True:
        data, peer = u.recvfrom(2048)
        log(f"udp echo {len(data)} bytes from {peer}")
        u.sendto(data, peer)


def tcp_echo_one(c):
    with c:
        while True:
            try:
                b = c.recv(65536)
            except OSError:
                return
            if not b:
                return
            c.sendall(b)


def tcp_echo():
    l = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    l.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    l.bind((BIND, PORT + 2))
    l.listen(16)
    while True:
        c, peer = l.accept()
        log(f"tcp echo from {peer}")
        threading.Thread(target=tcp_echo_one, args=(c,), daemon=True).start()


threading.Thread(target=udp_echo, daemon=True).start()
threading.Thread(target=tcp_echo, daemon=True).start()
# NW-7: 64 downloads connect at once; Python's default listen backlog of 5 made the host drop SYNs (1 s resend).
http.server.ThreadingHTTPServer.request_queue_size = 256
srv = http.server.ThreadingHTTPServer((BIND, PORT), H)
log(f"serving {len(DATA)} bytes sha256 {SHA} on {PORT}")
srv.serve_forever()
