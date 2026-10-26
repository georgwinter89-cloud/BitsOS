#!/usr/bin/env python3
"""Records what BusyBox writes to a 40x12 pty while keys are typed.

Usage: python3 -I capture.py user/ports/busybox/out/busybox crates/console/fixtures/busybox/NAME.bin NAME
The capture is the raw output bytes (after the host's ONLCR); a NUL byte marks each checkpoint.
"""
import fcntl, os, pty, select, struct, sys, termios, time, tempfile, shutil, signal

COLS, ROWS = 40, 12
LEFT, RIGHT, UP, DOWN = b"\x1b[D", b"\x1b[C", b"\x1b[A", b"\x1b[B"
BS, DEL, ESC = b"\x7f", b"\x1b[3~", b"\x1b"

SCENARIOS = {
    # Georg's case: Left x5, Backspace, Delete, then Enter.
    "georg": [("type", "echo Grüße xyz"), ("keys", LEFT * 5), ("keys", BS), ("keys", DEL), ("check",),
              ("keys", b"\r"), ("check",)],
    # Delete in the middle, then Backspace far from the end (more than 4 cells to move back).
    "middle": [("type", "echo abcdefghij"), ("keys", LEFT * 3), ("keys", DEL), ("check",),
               ("keys", LEFT * 2), ("keys", BS), ("check",), ("keys", b"\r"), ("check",)],
    # A line over two rows (prompt 2 + 45 cells), Left across the wrap, Backspace, Right back.
    "wrap": [("type", "echo 0123456789abcdefghijklmnopqrstuvwxyzABCD"), ("check",),
             ("keys", LEFT * 8), ("check",), ("keys", BS), ("check",), ("keys", RIGHT * 3), ("check",),
             ("keys", BS * 2), ("check",), ("keys", b"\r"), ("check",)],
    # History: a long (wrapping) line, a short one, then Up twice and Down once.
    "history": [("type", "echo long-0123456789-0123456789-0123456789\r"), ("type", "echo hi\r"), ("check",),
                ("keys", UP), ("check",), ("keys", UP), ("check",), ("keys", DOWN), ("check",)],
    # Ctrl+U before the cursor, Ctrl+K after it.
    "kill": [("type", "echo one two three"), ("keys", LEFT * 6), ("keys", b"\x15"), ("check",),
             ("keys", RIGHT * 2), ("keys", b"\x0b"), ("check",), ("keys", b"\r"), ("check",)],
    # vi: open a file, move, insert, then :q! back to the shell.
    "vi": [("type", "vi t.txt\r"), ("check",), ("keys", b"jjA"), ("type", " more"), ("keys", ESC), ("check",),
           ("keys", b":q!"), ("check",), ("keys", b"\r"), ("check",)],
}


def drain(fd, out, quiet=0.25, limit=3.0):
    end = time.time() + limit
    last = time.time()
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                b = os.read(fd, 4096)
            except OSError:
                return
            if not b:
                return
            out += b
            last = time.time()
        elif time.time() - last > quiet:
            return


def main():
    busybox, outpath, name = sys.argv[1], sys.argv[2], sys.argv[3]
    home = tempfile.mkdtemp(prefix="vtcap")
    with open(os.path.join(home, "t.txt"), "w") as f:
        f.write("first line\nsecond line\nthird line\n")
    os.mkdir(os.path.join(home, "bin"))
    os.symlink(busybox, os.path.join(home, "bin", "vi"))
    pid, fd = pty.fork()
    if pid == 0:
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        os.chdir(home)
        env = {"TERM": "linux", "HOME": home, "PATH": os.path.join(home, "bin"), "PS1": "$ ", "LINES": str(ROWS),
               "COLUMNS": str(COLS)}
        os.execve(busybox, ["ash"], env)
    out = bytearray()
    drain(fd, out, limit=2.0)
    # The prompt is there; drop the start-up bytes up to it so the fixture begins with the prompt.
    out = bytearray(out[out.rfind(b"$ "):])
    for step in SCENARIOS[name]:
        if step[0] == "type":
            for ch in step[1]:
                os.write(fd, ch.encode())
                drain(fd, out, quiet=0.05, limit=0.5)
        elif step[0] == "keys":
            # Escape sequences go as one write (one key); single bytes one by one.
            k = step[1]
            seqs = []
            while k:
                if k.startswith(b"\x1b[") and len(k) > 2:
                    n = 3 if k[2:3] != b"3" else 4
                    seqs.append(k[:n]); k = k[n:]
                else:
                    seqs.append(k[:1]); k = k[1:]
            for s in seqs:
                os.write(fd, s)
                drain(fd, out, quiet=0.1, limit=0.8)
        elif step[0] == "check":
            drain(fd, out, quiet=0.3)
            out += b"\0"
    os.kill(pid, signal.SIGKILL)
    os.waitpid(pid, 0)
    shutil.rmtree(home)
    with open(outpath, "wb") as f:
        f.write(out)


main()
