#!/usr/bin/env python3
"""NW-4 scenario client: the MCP lifeline over QEMU's hostfwd, against the real kernel stack (virtio-net or igb).

Usage:
  lifeline.py transport <port> <token>             V1's mcp-http-test.mjs transport cases (boot with mcp_conns=3)
  lifeline.py reserve <port> <token> <log> <sink>  1,000 kernel connections + no socket memory, MCP still takes 3
  lifeline.py busy <port> <token>                  a CPU spins 20 s (ktest nw.busy.spin), MCP answers meanwhile
  lifeline.py link <port> <token> <qmp> <netdev>   the cable out 10 s (QMP set_link), MCP back by itself
  lifeline.py up <port> <token> <seconds>          waits until MCP answers `log`

Exit status 0 = green; every step prints one line ("ok: ..." or "RED: ...")."""
import http.client
import json
import socket
import sys
import threading
import time

PING = '{"jsonrpc":"2.0","id":1,"method":"ping"}'


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"ok: {what}", flush=True)


# ------------------------------------------------------------------------------------------------------------
# MCP calls (one connection per request, as V1's mcp-call.mjs)

def call(port, token, name, args=None, timeout=30):
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
    try:
        body = {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args or {}}}
        c.request("POST", "/mcp", json.dumps(body), {"Authorization": f"Bearer {token}", "Content-Type": "application/json"})
        r = c.getresponse()
        data = r.read()
        if r.status != 200:
            raise ValueError(f"HTTP {r.status}")
        j = json.loads(data)
        if "result" not in j:
            raise ValueError(f"{j}")
        text = "\n".join(i.get("text", "") for i in j["result"]["content"] if i["type"] == "text")
        return j["result"]["isError"], text
    finally:
        c.close()


def wait_up(port, token, seconds, per_try=5):
    end = time.time() + seconds
    last = None
    while time.time() < end:
        try:
            err, _ = call(port, token, "log", {"max": 1}, timeout=per_try)
            if not err:
                return True
        except (OSError, http.client.HTTPException, ValueError) as e:
            last = e
        time.sleep(0.5)
    print(f"note: last error {last}")
    return False


# ------------------------------------------------------------------------------------------------------------
# Raw HTTP for the transport cases

def req(token="T", path="/mcp", method="POST", body=PING, headers=(), version="HTTP/1.1", eol="\r\n", length=True):
    h = [f"{method} {path} {version}", "Host: bitsos"]
    if token is not None:
        h.append(f"Authorization: Bearer {token}")
    h += list(headers)
    if body is not None and length:
        h.append(f"Content-Length: {len(body.encode())}")
    return (eol.join(h) + eol + eol + (body or "")).encode()


class Conn:
    def __init__(self, port, timeout=10):
        self.s = socket.create_connection(("127.0.0.1", port), timeout)
        self.s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = b""

    def send(self, data, one_byte=False):
        if one_byte:
            for i in range(len(data)):
                self.s.sendall(data[i:i + 1])
                time.sleep(0.002)
        else:
            self.s.sendall(data)

    def answer(self, timeout=10):
        """One HTTP answer: (status, headers dict, body) or None on timeout/close."""
        end = time.time() + timeout
        while b"\r\n\r\n" not in self.buf:
            if not self._more(end):
                return None
        head, rest = self.buf.split(b"\r\n\r\n", 1)
        lines = head.decode("latin-1").split("\r\n")
        status = int(lines[0].split()[1])
        hd = {}
        for l in lines[1:]:
            k, _, v = l.partition(":")
            hd[k.strip().lower()] = v.strip()
        n = int(hd.get("content-length", "0"))
        self.buf = rest
        while len(self.buf) < n:
            if not self._more(end):
                return None
        body, self.buf = self.buf[:n], self.buf[n:]
        return status, hd, body

    def _more(self, end):
        left = end - time.time()
        if left <= 0:
            return False
        self.s.settimeout(left)
        try:
            d = self.s.recv(65536)
        except (socket.timeout, OSError):
            return False
        if not d:
            return False
        self.buf += d
        return True

    def closed(self, timeout=5):
        """True when the server ends the connection (EOF or reset) within `timeout` with nothing more sent."""
        end = time.time() + timeout
        while time.time() < end:
            self.s.settimeout(max(0.01, end - time.time()))
            try:
                d = self.s.recv(4096)
            except socket.timeout:
                return False
            except OSError:
                return True
            if not d:
                return True
            self.buf += d
        return False

    def close(self):
        self.s.close()


def ask(port, data, timeout=10, one_byte=False):
    c = Conn(port)
    try:
        c.send(data, one_byte)
        a = c.answer(timeout)
        return a, c.closed(5) if a else True
    finally:
        c.close()


def ok_ping(a):
    return a is not None and a[0] == 200 and b'"result"' in a[2]


def transport(port, token):
    need(wait_up(port, token, 120), "MCP answers")
    a, closed = ask(port, req(token=None))
    need(a and a[0] == 401 and a[1].get("www-authenticate") == "Bearer" and closed, "401 without the token, closed", a)
    a, closed = ask(port, req(token="wrong-token-123"))
    need(a and a[0] == 401 and closed, "401 with a wrong token", a)
    a, closed = ask(port, req(token, headers=["Origin: http://evil.example"]))
    need(a and a[0] == 403 and closed, "403 for an Origin header, closed", a)
    a, closed = ask(port, req(token, path="/other"))
    need(a and a[0] == 404 and closed, "404 for another path, closed", a)
    a, closed = ask(port, req(token, method="GET", body=None))
    need(a and a[0] == 405 and a[1].get("allow") == "POST" and closed, "405 for GET with Allow: POST", a)
    a, closed = ask(port, req(token, body=None))
    need(a and a[0] == 411 and closed, "411 without a length", a)
    a, closed = ask(port, req(token, body=None, headers=["Content-Length: 262145"]))
    need(a and a[0] == 413 and closed, "413 above 256 KB", a)
    a, closed = ask(port, req(token, headers=["X-Long: " + "a" * 17000]))
    need(a and a[0] == 431 and closed, "431 for a head above 16 KB", a)
    a, closed = ask(port, req(token, headers=["No colon here"]))
    need(a and a[0] == 400 and closed, "400 for a broken header line", a)
    a, _ = ask(port, req(token), one_byte=True)
    need(ok_ping(a), "a request in 1-byte writes", a)
    a, _ = ask(port, req(token, eol="\n"))
    need(ok_ping(a), "LF line ends", a)
    c = Conn(port)
    c.send(req(token))
    a1 = c.answer()
    c.send(req(token) + req(token))
    a2, a3 = c.answer(), c.answer()
    need(ok_ping(a1) and ok_ping(a2) and ok_ping(a3) and not c.closed(0.5), "keep-alive and pipelining on one connection")
    c.close()
    a, closed = ask(port, req(token, headers=["Connection: close"]))
    need(ok_ping(a) and closed, "Connection: close answered, then closed", a)
    a, closed = ask(port, req(token, version="HTTP/1.0"))
    need(ok_ping(a) and closed, "HTTP/1.0 answered, then closed", a)
    # Three connections side by side (mcp_conns=3), each served.
    cs = [Conn(port) for _ in range(3)]
    for c in cs:
        c.send(req(token))
    need(all(ok_ping(c.answer()) for c in cs), "three connections side by side")
    # A fourth knocks: the idlest gives way (V1's knock/evict, a real backlog in V2).
    time.sleep(1.5)
    d = Conn(port)
    d.send(req(token))
    need(ok_ping(d.answer(15)), "a knocking fourth connection is served")
    need(sum(1 for c in cs if c.closed(3)) >= 1, "the idlest connection gave way")
    for c in cs + [d]:
        c.close()
    # A request that never completes: 408 after 10 s.
    c = Conn(port)
    c.send(req(token)[:30])
    a = c.answer(20)
    need(a is not None and a[0] == 408 and c.closed(5), "408 for a request not complete in 10 s", a)
    c.close()
    # An idle connection is closed after 60 s.
    c = Conn(port)
    c.send(req(token))
    need(ok_ping(c.answer()), "idle connection served first")
    t0 = time.time()
    need(c.closed(80) and 55 <= time.time() - t0 <= 75, f"idle close after {time.time() - t0:.0f} s")
    c.close()


# ------------------------------------------------------------------------------------------------------------
# The MCP reserve: the guest's ktest nw.reserve.pressure holds 1,000 connections to our sink and takes all socket
# memory away; MCP must still accept 3 connections side by side and answer `log`, three rounds in a row.

def sink(port, stop):
    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", port))
    srv.listen(4096)
    srv.settimeout(0.5)
    held = []
    while not stop.is_set():
        try:
            s, _ = srv.accept()
            held.append(s)
        except socket.timeout:
            pass
    print(f"note: sink held {len(held)} connections")
    for s in held:
        s.close()
    srv.close()


def wait_log(path, pattern, seconds):
    end = time.time() + seconds
    while time.time() < end:
        try:
            with open(path, "rb") as f:
                if pattern.encode() in f.read():
                    return True
        except OSError:
            pass
        time.sleep(0.2)
    return False


def reserve(port, token, log, sink_port):
    stop = threading.Event()
    threading.Thread(target=sink, args=(sink_port, stop), daemon=True).start()
    need(wait_up(port, token, 120), "MCP answers")
    result = {}

    def run_ktest():
        try:
            result["r"] = call(port, token, "ktest", {"filter": "nw.reserve.pressure", "timeout_ms": 240000}, timeout=260)
        except Exception as e:  # noqa: BLE001 - reported below
            result["e"] = e

    t = threading.Thread(target=run_ktest)
    t.start()
    need(wait_log(log, "ktest.reserve_pressure_ready", 180), "guest holds 1,000 connections and refuses socket memory")
    for rnd in range(3):
        cs = [Conn(port, 20) for _ in range(3)]
        body = json.dumps({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "log", "arguments": {"max": 3}}})
        for c in cs:
            c.send(req(token, body=body))
        answers = [c.answer(30) for c in cs]
        need(all(a is not None and a[0] == 200 and b'"isError":false' in a[2] for a in answers),
             f"round {rnd + 1}: 3 connections side by side, each answered `log`", answers)
        for c in cs:
            c.close()
        time.sleep(1)
    t.join(300)
    stop.set()
    need("r" in result and not result["r"][0] and "failed=0" in result["r"][1] and "passed=1" in result["r"][1],
         "ktest nw.reserve.pressure passed", result)


# ------------------------------------------------------------------------------------------------------------
# A CPU busy 20 s (no yield): MCP keeps answering on another connection.

def busy(port, token):
    need(wait_up(port, token, 120), "MCP answers")
    result = {}

    def spin():
        try:
            result["r"] = call(port, token, "ktest", {"filter": "nw.busy.spin", "timeout_ms": 60000}, timeout=90)
        except Exception as e:  # noqa: BLE001
            result["e"] = e

    t = threading.Thread(target=spin)
    t.start()
    time.sleep(2)
    worst, n = 0.0, 0
    end = time.time() + 15
    while time.time() < end:
        t0 = time.time()
        err, _ = call(port, token, "log", {"max": 1}, timeout=10)
        need_ok = not err
        if not need_ok:
            raise SystemExit("RED: log failed while a CPU spins")
        worst = max(worst, time.time() - t0)
        n += 1
        time.sleep(0.2)
    t.join(120)
    need(n >= 20, f"{n} log calls answered while a CPU spun (worst {worst * 1000:.0f} ms; QEMU timing not judged)")
    need("r" in result and "failed=0" in result["r"][1] and "passed=1" in result["r"][1], "ktest nw.busy.spin passed", result)


# ------------------------------------------------------------------------------------------------------------
# The cable out for 10 s and back.

def qmp(path, netdev, up):
    s = socket.socket(socket.AF_UNIX)
    s.connect(path)
    f = s.makefile("rw")
    f.readline()
    for c in ({"execute": "qmp_capabilities"}, {"execute": "set_link", "arguments": {"name": netdev, "up": up}}):
        f.write(json.dumps(c) + "\n")
        f.flush()
        r = json.loads(f.readline())
        while "event" in r:
            r = json.loads(f.readline())
        if "error" in r:
            raise SystemExit(f"RED: QMP {c}: {r}")
    s.close()


def msi_counts(port, token):
    err, t = call(port, token, "hw_info", {"section": "msi"})
    return t if not err else ""


def link(port, token, qmp_path, netdev):
    need(wait_up(port, token, 120), "MCP answers")
    before = msi_counts(port, token)
    print("note: " + " | ".join(l.strip() for l in before.splitlines() if "count" in l))
    qmp(qmp_path, netdev, False)
    print("note: cable out", flush=True)
    time.sleep(10)
    qmp(qmp_path, netdev, True)
    t0 = time.time()
    print("note: cable back", flush=True)
    need(wait_up(port, token, 60, per_try=3), f"MCP back by itself {time.time() - t0:.1f} s after the cable came back")
    err, t = call(port, token, "net_info")
    need(not err and "addr=10.0.2.15/24" in t, "the lease is back", t)
    after = msi_counts(port, token)
    print("note: " + " | ".join(l.strip() for l in after.splitlines() if "count" in l))


def main():
    mode, port, token = sys.argv[1], int(sys.argv[2]), sys.argv[3]
    if mode == "transport":
        transport(port, token)
    elif mode == "reserve":
        reserve(port, token, sys.argv[4], int(sys.argv[5]))
    elif mode == "busy":
        busy(port, token)
    elif mode == "link":
        link(port, token, sys.argv[4], sys.argv[5])
    elif mode == "up":
        need(wait_up(port, token, int(sys.argv[4])), "MCP answers")
    else:
        raise SystemExit(f"unknown mode {mode}")


if __name__ == "__main__":
    main()
