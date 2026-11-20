#!/usr/bin/env python3
"""PK-6f: a mirror that answers every request only after SLEEP seconds, then 404 (the slow mirror of 96-bitspkgd.sh).
Usage: pk_slow_mirror.py <port> <sleep_s>. Binds 127.0.0.1 only; stopped by its PID."""
import http.server
import socketserver
import sys
import time

sys.dont_write_bytecode = True
PORT, SLEEP = int(sys.argv[1]), float(sys.argv[2])


class Slow(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        time.sleep(SLEEP)
        self.send_error(404, "slow fixture mirror")

    def log_message(self, fmt, *args):
        sys.stderr.write("slow-mirror: " + (fmt % args) + "\n")


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True


Server(("127.0.0.1", PORT), Slow).serve_forever()
