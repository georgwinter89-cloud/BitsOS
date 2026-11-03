#!/usr/bin/env python3
"""MC-9b: a stand-in for bitsinstall on the host (the real one needs V2's gate). It listens on SOCKET, takes one
confirmer (bitscomp), speaks the protocol of user/services/bitsinstall/src/confirm.rs and decides as decide.rs does:
a running grant of the kind answers yes without a dialog; otherwise the dialog, where only a yes with
input=physical counts (injected or unnamed input: refused) and no answer is no; a grant is taken only with
input=physical, for a grantable kind, at most 24 h ahead. Then it runs the scenario's requests in order and writes
every line it gets and every decision to LOG ("decided ID yes|no by=...").

  fake_installer.py SOCKET LOG
"""
import json
import os
import socket
import sys
import time

GRANTABLE = {"install", "pin", "reload", "reboot", "settings"}


class Fake:
    def __init__(self, path, log):
        self.log_path = log
        self.grants = {}
        self.granted = 0
        srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        srv.bind(path)
        os.chmod(path, 0o666)
        srv.listen(1)
        self.say("listening")
        srv.settimeout(30)
        self.conn, _ = srv.accept()
        self.buf = b""
        self.say("confirmer connected")
        self.send({"type": "hello", "version": 1, "grants": self.grant_list()})

    def say(self, line):
        with open(self.log_path, "a") as f:
            f.write(line + "\n")

    def send(self, obj):
        self.conn.sendall((json.dumps(obj, separators=(",", ":"), ensure_ascii=False) + "\n").encode())

    def grant_list(self):
        now = time.time()
        for k in [k for k, u in self.grants.items() if u <= now]:
            del self.grants[k]
            self.say(f"grant {k} expired")
        return [{"kind": k, "until": u} for k, u in sorted(self.grants.items())]

    def lines(self, deadline):
        """Yields the confirmer's messages until `deadline`; grants, revokes and lists are handled here."""
        while True:
            while b"\n" in self.buf:
                raw, self.buf = self.buf.split(b"\n", 1)
                self.say("got " + raw.decode(errors="replace"))
                try:
                    m = json.loads(raw)
                except ValueError:
                    continue
                if self.handle(m):
                    yield m
            left = deadline - time.time()
            if left <= 0:
                return
            self.conn.settimeout(left)
            try:
                chunk = self.conn.recv(65536)
            except socket.timeout:
                return
            if not chunk:
                self.say("confirmer gone")
                return
            self.buf += chunk

    def handle(self, m):
        """False for what the fake answers itself (grant, revoke, list); True for an answer."""
        t = m.get("type")
        if t == "grant":
            until, now = m.get("until", 0), time.time()
            ok = m.get("input") == "physical" and m.get("kind") in GRANTABLE and now < until <= now + 86400
            if ok:
                self.grants[m["kind"]] = until
                self.granted += 1
                self.say(f"grant {m['kind']} set for {until - now:.1f} s (grant #{self.granted})")
                self.send({"type": "grants", "grants": self.grant_list()})
            else:
                self.say("grant refused")
                self.send({"type": "refused", "what": "grant", "reason": "nicht übernommen"})
            return False
        if t == "revoke":
            self.grants.clear() if m.get("kind") == "all" else self.grants.pop(m.get("kind"), None)
            self.say(f"revoked {m.get('kind')}")
            self.send({"type": "grants", "grants": self.grant_list()})
            return False
        if t == "list":
            self.send({"type": "grants", "grants": self.grant_list()})
            return False
        return t == "answer"

    def request(self, rid, kind, title, text, timeout_ms, cancel_after=None, extra=()):
        """`extra`: raw lines sent right after the ask (a bad line, a second ask while this one is shown)."""
        self.grant_list()
        if kind in self.grants:
            self.say(f"decided {rid} yes by=grant")
            return
        self.send({"type": "ask", "id": rid, "kind": kind, "title": title, "text": text, "timeout_ms": timeout_ms})
        for raw in extra:
            self.conn.sendall(raw)
        wait = (cancel_after if cancel_after is not None else timeout_ms / 1000) + 0.5
        for m in self.lines(time.time() + wait):
            if m.get("id") != rid:
                self.say(f"answer for {m.get('id')} while asking {rid}: refused")
                continue
            if m.get("yes") is True and m.get("input") == "physical":
                self.say(f"decided {rid} yes by=dialog")
            elif m.get("yes") is True:
                self.say(f"decided {rid} no by=refused input={m.get('input')}")
            else:
                self.say(f"decided {rid} no by=dialog input={m.get('input')}")
            return
        self.say(f"decided {rid} no by=timeout")
        self.send({"type": "cancel", "id": rid, "reason": "no answer" if cancel_after is None else "test cancel"})

    def wait_revoke(self, seconds):
        deadline = time.time() + seconds
        while time.time() < deadline and self.grants:
            for _ in self.lines(min(deadline, time.time() + 0.2)):
                pass
        self.say("grants revoked" if not self.grants else "no revoke came")

    def wait_grant(self, seconds):
        # By count, not by `self.grants`: a revoke may follow the grant within one read window.
        deadline, before = time.time() + seconds, self.granted
        while time.time() < deadline and self.granted == before:
            for _ in self.lines(min(deadline, time.time() + 0.2)):
                pass


def main():
    sock, log = sys.argv[1], sys.argv[2]
    f = Fake(sock, log)
    text = "Werkzeug: reboot\nArgumente: dry_run = true\nFragt: MCP-Gegenstelle 10.0.2.2\nAlle Programme enden; der Rechner startet neu."
    f.request(1, "reboot", "Rechner neu starten", text, 20000)
    f.request(2, "reboot", "Rechner neu starten", text, 20000)
    second = {"type": "ask", "id": 103, "kind": "install", "title": "Zweite Frage", "text": "x", "timeout_ms": 20000}
    f.request(3, "reboot", "Rechner neu starten", text, 20000,
              extra=[b"{not json\n", (json.dumps(second) + "\n").encode()])
    # The longest text the installer sends (TEXT_MAX 4000 characters), with an argument of 1500 characters
    # without a break and many short lines.
    long_text = "Werkzeug: reload\nArgumente: image = /" + "a" * 1500 + "\n"
    while len(long_text) < 3990:
        long_text += "Zeile mit Umlauten äöü ß\n"
    f.request(4, "reload", "Neuen Kernel jetzt laden (Reload)", long_text[:4000], 1500)
    f.request(5, "pin", "Startende Generation festlegen", "Werkzeug: system_pin", 20000, cancel_after=1.0)
    f.say("waiting for a grant")
    f.wait_grant(40)
    until = f.grants.get("install")
    f.request(6, "install", "Neuen Kernel installieren", "Werkzeug: install_kernel", 20000)
    if until:
        time.sleep(max(0.0, until - time.time()) + 1.5)
    f.request(7, "install", "Neuen Kernel installieren", "Werkzeug: install_kernel", 20000)
    f.say("waiting for grant #2")
    f.wait_grant(40)
    f.wait_revoke(40)
    f.say("DONE")
    for _ in f.lines(time.time() + 30):
        pass


if __name__ == "__main__":
    main()
