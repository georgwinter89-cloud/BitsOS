#!/usr/bin/env python3
"""MC-10: a stand-in for bitsinstall's confirmer socket on the host, for bitscomp's access dialog (97-access-dialog.sh).
It takes one confirmer, speaks user/services/bitsinstall/src/confirm.rs's protocol and runs a fixed order:
ask 11 (kind access) and log its answer; ask 12 and log its answer; show a live grant (the bar's chip) and wait
for the revoke at the keyboard; show none. Every line it gets and every step goes to LOG.

  access_stand_in.py SOCKET LOG CLIENT_FP
"""
import json
import os
import socket
import sys
import time


def main():
    path, log, fp = sys.argv[1], sys.argv[2], sys.argv[3]

    def say(line):
        with open(log, "a") as f:
            f.write(line + "\n")

    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(path)
    os.chmod(path, 0o666)
    srv.listen(1)
    srv.settimeout(30)
    conn, _ = srv.accept()
    say("confirmer connected")
    buf = b""

    def send(obj):
        conn.sendall((json.dumps(obj, separators=(",", ":"), ensure_ascii=False) + "\n").encode())

    def wait(pred, secs, what):
        nonlocal buf
        end = time.time() + secs
        conn.settimeout(0.2)
        while time.time() < end:
            while b"\n" in buf:
                raw, buf = buf.split(b"\n", 1)
                say("got " + raw.decode(errors="replace"))
                try:
                    m = json.loads(raw)
                except ValueError:
                    continue
                if pred(m):
                    return m
            try:
                chunk = conn.recv(4096)
            except socket.timeout:
                continue
            if not chunk:
                say("FAIL: the confirmer closed the connection waiting for " + what)
                sys.exit(1)
            buf += chunk
        say("FAIL: no " + what)
        sys.exit(1)

    clients = [{"client": fp, "name": "bitsos-desk"}]
    send({"type": "hello", "version": 1, "grants": [], "access": [], "clients": clients})
    text = "Der Gaming-PC (10.0.2.2) bittet um Vollzugriff für 24 h.\n\nText vom Gaming-PC (ungeprüft): „Welle 21“"
    for qid in (11, 12):
        send({"type": "ask", "id": qid, "kind": "access", "title": "", "text": text, "timeout_ms": 120000, "client": fp, "name": "bitsos-desk", "hours": 24})
        m = wait(lambda m, q=qid: m.get("id") == q and m.get("type") in ("access", "answer"), 60, f"answer to {qid}")
        say(f"answer {qid} type={m.get('type')} input={m.get('input')} hours={m.get('hours')} nvme={m.get('nvme')} client_ok={m.get('client') == fp}")
    until = int(time.time()) + 3600
    send({"type": "access", "access": [{"client": fp, "name": "bitsos-desk", "until": until, "nvme": False}], "clients": clients})
    say("grant shown")
    wait(lambda m: m.get("type") == "revoke" and m.get("kind") == "access", 60, "revoke at the keyboard")
    say("revoked")
    send({"type": "access", "access": [], "clients": clients})
    say("DONE")
    time.sleep(30)


main()
