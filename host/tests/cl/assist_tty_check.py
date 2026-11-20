#!/usr/bin/env python3
"""CL-3a on the host: the full-screen chat of `assist` driven through a pseudo-terminal, against a real settingsd
and the fake provider. Typing with umlauts, the streamed answer, the yes/no question before a change, scrolling,
line editing, Esc and Ctrl-D, and the terminal handed back as it was.

Usage: assist_tty_check.py <assist> <settings> <socket> <fake port> <fake key> <fake log> <run dir>
"""
import fcntl
import json
import os
import pty
import select
import struct
import subprocess
import sys
import termios
import time

ASSIST, SETTINGS, SOCK, PORT, KEY, FAKE_LOG, RUN_DIR = sys.argv[1:8]
COLS, ROWS = 110, 14


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}{': ' + str(detail)[-1500:] if detail else ''}")
    print(f"ok: {what}")


def settings(*args, stdin=None):
    r = subprocess.run([SETTINGS, "--socket", SOCK, "--json", *args], capture_output=True, text=True, input=stdin, timeout=30)
    line = next((l for l in r.stdout.splitlines() if l.startswith("{")), "{}")
    return json.loads(line)


def set_(key, value):
    a = settings("set", key, value)
    need(a.get("ok") is True, f"settings set {key}", a)


def fake_log():
    with open(FAKE_LOG) as f:
        return [json.loads(l) for l in f if l.strip()]


class Chat:
    def __init__(self):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.execv(ASSIST, [ASSIST, "--socket", SOCK, "--log", os.path.join(RUN_DIR, "assist-chat.log")])
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        self.seen = b""

    def read(self, secs):
        end = time.time() + secs
        while time.time() < end:
            r, _, _ = select.select([self.fd], [], [], 0.05)
            if r:
                try:
                    data = os.read(self.fd, 65536)
                except OSError:
                    return False
                if not data:
                    return False
                self.seen += data
        return True

    def wait(self, text, what, secs=20):
        """Waits until `text` was drawn after the last call's mark."""
        end = time.time() + secs
        while time.time() < end:
            if text.encode() in self.seen[self.mark:]:
                print(f"ok: {what}")
                return
            self.read(0.1)
        raise SystemExit(f"RED: {what}: {text!r} never appeared; the screen got:\n{self.seen[self.mark:][-2500:].decode('utf-8', 'replace')!r}")

    mark = 0

    def type(self, text):
        self.mark = len(self.seen)
        os.write(self.fd, text.encode())

    def end(self, what):
        end = time.time() + 10
        while time.time() < end:
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                need(os.WIFEXITED(status) and os.WEXITSTATUS(status) == 0, what, status)
                return
            self.read(0.1)
        os.kill(self.pid, 9)
        raise SystemExit(f"RED: {what}: assist did not end")


def main():
    # Nothing configured: the chat opens, explains, and Esc leaves it.
    c = Chat()
    c.wait("kein KI-Anbieter eingerichtet", "without a provider the chat explains how to add one")
    c.wait("assist --add NAME", "... with the commands")
    need(b"\x1b[?1049h" in c.seen, "the chat uses the alternate screen")
    c.type("\x1b")
    c.end("Esc leaves the chat")
    need(c.seen.rstrip().endswith(b"\x1b[?1049l"), "the console's own screen is back afterwards", c.seen[-40:])

    r = subprocess.run([ASSIST, "--socket", SOCK, "--add", "claude"], capture_output=True, text=True, timeout=30)
    need(r.returncode == 0, "assist --add claude", r.stderr)
    set_("ai.provider.claude.kind", "anthropic")
    set_("ai.provider.claude.address", f"http://127.0.0.1:{PORT}")
    set_("ai.provider.claude.model", "claude-opus-5-5")
    set_("ai.provider.claude.place", "local")
    set_("ai.role.chat", "claude")
    r = subprocess.run([ASSIST, "--socket", SOCK, "--key", "claude"], capture_output=True, text=True, input=KEY + "\n", timeout=30)
    need(r.returncode == 0 and KEY not in r.stdout + r.stderr, "assist --key claude stores the key without printing it", r.stderr)

    c = Chat()
    c.wait("claude · claude-opus-5-5 · lokal", "the status line names provider, model and place")
    # A question with umlauts, edited on the way: typed wrong, cursor back, fixed.
    c.type("Grüße, wie gehts?")
    c.wait("> Grüße, wie gehts?", "umlauts appear in the input line as typed")
    c.type("\x1b[D\x1b[D\x7f\x7f\x7f\x7fgeht es")
    c.wait("> Grüße, wie geht ess?", "editing in the middle of the line")
    c.type("\x15Hallo, grüß dich\r")
    c.wait("Du: Hallo, grüß dich", "the question is shown")
    c.wait("ANTHROPIC-OK", "the streamed answer arrives")
    c.wait("äöüß", "... with its umlauts")

    # A change: the chat asks first, in German, with the old and the new value. The provider is slow here and
    # a j is typed while its answer is still on the way: a key from before the question is no yes.
    c.type("TOOL LANGSAM bitte\r")
    c.read(0.4)
    os.write(c.fd, b"j")
    c.wait("Einstellung „theme.scale“ ändern: 125 → 150", "the chat says what is about to change")
    c.wait("Erlauben? (j = ja, n = nein)", "... and asks")
    c.read(0.6)
    need(settings("get", "theme.scale").get("value") == 125, "a j typed before the question was shown changed nothing")
    need(b"ANTHROPIC-DONE" not in c.seen[c.mark:] and b"nein) ja" not in c.seen[c.mark:], "... and the question is still open")
    c.type("j")
    c.wait("ANTHROPIC-DONE", "after j the tool ran and the model finished")
    need(settings("get", "theme.scale").get("value") == 150, "theme.scale is 150 now")

    # The same for taking it back, answered with n: nothing changes and the model is told. (Ctrl-U first: the
    # early j went into the input line.)
    c.type("\x15TOOL-UNDO\r")
    c.wait("Änderung zurücknehmen: „theme.scale“: 150 → 125", "the undo is described too")
    c.read(0.3)
    c.type("n")
    c.wait("ANTHROPIC-DONE", "after n the model still gets its answer")
    need(settings("get", "theme.scale").get("value") == 150, "theme.scale stayed 150")
    told = [e for e in fake_log() if any("Georg said no" in t for t in e.get("tool_results", []))]
    need(len(told) == 1, "the model was told that Georg said no", fake_log()[-2:])

    # Scrolling back and forth.
    c.type("\x1b[5~")
    c.wait("Zeilen weiter unten", "Bild↑ scrolls back")
    c.type("\x1b[6~\x1b[6~\x1b[6~")
    c.read(0.3)
    # Over a line being typed, Esc warns first; an arrow key arriving in two pieces is not an Esc.
    c.type("Entwurf\x1b")
    c.wait("noch einmal Esc beendet", "Esc over a draft only warns")
    c.type("\x1b")
    time.sleep(0.05)
    os.write(c.fd, b"[D!")
    c.wait("> Entwur!f", "a split arrow key moves the cursor and the chat goes on")
    c.type("\x04")
    c.wait("noch einmal Esc beendet", "Ctrl-D over a draft warns as well")
    c.type("\x04")
    c.end("the second Ctrl-D leaves the chat")

    log = fake_log()
    reqs = [e for e in log if "kind" in e]
    need(reqs and all(e["key_ok"] for e in reqs), "every request carried the right key", [e for e in reqs if not e["key_ok"]])
    need(all(len(e["tools_offered"]) == 6 for e in reqs), "the six settings tools were offered each time")
    # EINS K3: the console chat has the same native tools as every chat, under the same one level (K2).
    need(all(len(e["native_tools"]) == 10 for e in reqs), "... and the ten native tools (befehl ... loeschen)", [e["native_tools"] for e in reqs])
    need(all(e["level_named"] == "Fragt vorher" for e in reqs), "... and the one prompt names the level Fragt vorher", [e["level_named"] for e in reqs])
    shown = c.seen.decode("utf-8", "replace")
    chat_log = open(os.path.join(RUN_DIR, "assist-chat.log")).read() if os.path.exists(os.path.join(RUN_DIR, "assist-chat.log")) else ""
    need(KEY not in shown and KEY not in chat_log and KEY not in open(FAKE_LOG).read(), "the key appeared on no screen and in no log")
    hist = settings("history", "50")
    need(KEY not in json.dumps(hist), "... and not in settingsd's history")
    print("assist chat on a terminal: green")


if __name__ == "__main__":
    main()
