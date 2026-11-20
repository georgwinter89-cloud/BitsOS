#!/usr/bin/env python3
"""BR-9 on V2 through MCP (QEMU hostfwd): a fake brave-bin (host/tests/pk/browser-probe.c, Arch's layout) installs
into a scratch store as uid 1000 and starts through /bin/bitsrun as the library starts brave-browser.desktop
(`--exec /usr/bin/brave brave-bin`): the binary runs, not the start script, with the stick launcher's flags and
environment and the sofa's arguments in the launcher's form; Chromium's namespace sandbox calls work inside the
view; the stick's profile (/data/appdata/brave of the store) is taken over once, `--plan` only says so; a second
start hands its address to the first one through its singleton socket under TMPDIR; removal.

Usage: pk_browser_check.py <host port> <token> <tar of pkg/>
Exit status 0 = green."""
import base64
import http.client
import json
import sys
import time

PORT, TOKEN, TAR = int(sys.argv[1]), sys.argv[2], sys.argv[3]
BP, BR, BB = "/bin/bitspkg", "/bin/bitsrun", "/bin/busybox"
W = "/tmp/pkb"
R = W + "/root"
H = W + "/home"
ENV = ["PATH=/bin:/sbin:/usr/bin", f"HOME={H}", "XDG_RUNTIME_DIR=/run/user/1000", "LD_LIBRARY_PATH=/bad", "WAYLAND_DISPLAY=wayland-bits-0"]
next_id = 0
fail = False


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    c.request("POST", "/mcp", json.dumps({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}}),
              {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    j = json.loads(r.read() or b"null")
    if r.status != 200 or not j or "result" not in j:
        raise ValueError(f"{method}: HTTP {r.status} {j}")
    return j["result"]


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=120)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "br9", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def call(c, name, args):
    r = rpc(c, "tools/call", {"name": name, "arguments": args})
    return r["isError"], "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


def need(cond, what, detail=""):
    global fail
    print(f"ok   {what}" if cond else f"FAIL {what}\n{detail[:3000]}")
    fail = fail or not cond


def put(c, path, data):
    step = 96 * 1024
    for off in range(0, len(data), step):
        err, t = call(c, "write_file", {"path": path, "data": base64.b64encode(data[off:off + step]).decode(), "offset": off})
        if err:
            raise SystemExit(f"RED: write_file {path} at {off}: {t}")


def run(c, argv, env=None, timeout_ms=60000):
    """(exit status line, stdout, stderr) of a program run over MCP (uid 1000)."""
    a = {"path": argv[0], "args": argv[1:], "timeout_ms": timeout_ms, "kill_after_ms": timeout_ms}
    if env:
        a["env"] = env
    err, t = call(c, "run", a)
    if err or "\n--- stdout (" not in t:
        return None, t, t
    head, rest = t.split("\n--- stdout (", 1)
    out, errpart = rest.split("\n--- stderr (", 1)
    body = lambda part: part.split(") ---", 1)[1].removeprefix("\n")
    return head.splitlines()[0], body(out), body(errpart)


def ok(st):
    return bool(st) and st.endswith("exit 0")


FLAGS = ("--ozone-platform=wayland --use-gl=angle --use-angle=gl "
         "--enable-features=AcceleratedVideoDecoder,AcceleratedVideoDecodeLinuxGL,VaapiIgnoreDriverChecks "
         "--password-store=basic --disable-breakpad --disable-crash-reporter "
         "--no-first-run --no-default-browser-check --autoplay-policy=no-user-gesture-required --enable-wayland-ime "
         "--wayland-text-input-version=3")
STICK = R + "/data/appdata/brave"
PROFILE = R + "/data/appdata/brave-bin/config/BraveSoftware/Brave-Browser"


def values(out):
    v = {}
    for l in out.splitlines():
        if " = " in l:
            k, x = l.split(" = ", 1)
            v.setdefault(k, x)
    return v


def sh(c, cmd, timeout_ms=60000):
    return run(c, [BB, "sh", "-c", cmd], ENV, timeout_ms)


def hour(v, default):
    """An hour setting as bitsrun's browser.rs (and the launcher) read it: digits only, at most 23."""
    v = (v or "").strip()
    return min(int(v), 23) if v.isdigit() else default


def desktop(c):
    """(dark, accessibility bus) of the image as bitsrun asks them: settingsd's theme (theme.mode, for "auto" the
    hour and its bounds) and atspid's socket. The expectations follow the image; neither state is assumed."""
    q = "; ".join(f'v=$(settings get {k} 2>/dev/null) && printf "{k}=%s\\n" "$(echo "$v" | head -n 1)"'
                  for k in ("theme.mode", "theme.auto.light_from", "theme.auto.dark_from"))
    st, out, err = sh(c, q + '; printf "hour=%s\\n" "$(date +%H)"; [ -S /run/user/1000/at-spi/bus ] && echo bus=yes || echo bus=no')
    if not ok(st):
        raise SystemExit(f"RED: the image's theme and accessibility bus are not readable: {out}{err}")
    v = dict(l.split("=", 1) for l in out.splitlines() if "=" in l)
    mode = v.get("theme.mode", "").strip()
    if mode == "dark":
        dark = True
    elif mode == "auto":
        h, f, t = hour(v.get("hour"), 0), hour(v.get("theme.auto.light_from"), 7), hour(v.get("theme.auto.dark_from"), 19)
        dark = (h < f or h >= t) if f <= t else (h < f and h >= t)
    else:
        dark = False
    print(f"image: theme.mode={mode or '-'} dark={dark} accessibility bus={v.get('bus')}")
    return dark, v.get("bus") == "yes"


def main():
    c = connect(240)
    st, out, err = run(c, [BB, "test", "-x", "/bin/bwrap"])
    if not ok(st):
        raise SystemExit(f"RED: no /bin/bwrap in the image: {st} {out}{err}")
    st, out, err = run(c, [BB, "test", "-d", "/run/user/1000"])
    need(ok(st), "the user's runtime folder /run/user/1000 exists (TMPDIR of the browser)", f"{st}\n{out}{err}")
    run(c, [BB, "mkdir", "-p", W, H])
    put(c, W + "/pkg.tar", open(TAR, "rb").read())
    st, out, err = run(c, [BB, "tar", "-xf", W + "/pkg.tar", "-C", W])
    need(ok(st), "the package is unpacked on V2", f"{st}\n{out}{err}")
    st, out, err = run(c, [BP, "--root", R, "install", f"{W}/pkg"])
    need(ok(st), "install the fake brave-bin (uid 1000)", f"{st}\n{out}{err}")
    st, out, err = sh(c, f"mkdir -p '{STICK}/Default' && echo '{{}}' > '{STICK}/Local State' && printf stick-bookmarks > '{STICK}/Default/Bookmarks'"
                         f" && chmod 700 '{STICK}' && ln -s host-4000000 '{STICK}/SingletonLock'")
    need(ok(st), "a stick profile with a stale lock (as the stick's Brave leaves it)", f"{st}\n{out}{err}")
    dark, bus = desktop(c)
    flags = FLAGS + (" --force-dark-mode" if dark else "") + (" --force-renderer-accessibility" if bus else "")
    lib = [BR, "--store", R, "--exec", "/usr/bin/brave"]
    st, out, err = run(c, lib + ["--plan", "brave-bin"], ENV)
    print(out.rstrip())
    runline = next((l for l in out.splitlines() if l.startswith("run ")), "")
    need(ok(st) and runline == f"run /opt/brave-bin/brave {flags}", "--plan: the binary with the launcher's flags", f"{st}\n{out}{err}")
    need("env TMPDIR=/run/user/1000" in out and "env GTK_USE_PORTAL=0" in out and "wird beim Start nach" in out, "--plan: TMPDIR, the environment, the takeover said", out)
    st2, o2, _ = run(c, [BB, "test", "-f", f"{STICK}/Local State"])
    need(ok(st2), "--plan moved nothing", o2)
    st, out, err = run(c, lib + ["brave-bin", "https://a.test/", "--", "--start-fullscreen"], ENV)
    print(out.rstrip())
    v = values(out)
    need(ok(st) and v.get("exe") == "/opt/brave-bin/brave" and "wrapper" not in v, "the binary runs, not the start script", f"{st}\n{out}{err}")
    need(v.get("uid") == "1000", "as uid 1000", out)
    need(v.get("args") == f"{flags} https://a.test/ --start-fullscreen",
         "the flags, then the address and the sofa's argument (the lone -- dropped)", v.get("args", "-"))
    a11y = ([("AT_SPI_BUS_ADDRESS", "unix:path=/run/user/1000/at-spi/bus"), ("ACCESSIBILITY_ENABLED", "1"), ("NO_AT_BRIDGE", "(unset)")] if bus
            else [("NO_AT_BRIDGE", "1"), ("AT_SPI_BUS_ADDRESS", "(unset)"), ("ACCESSIBILITY_ENABLED", "(unset)")])
    for k, want in a11y + [("TMPDIR", "/run/user/1000"), ("GTK_USE_PORTAL", "0"), ("DBUS_SESSION_BUS_ADDRESS", "disabled:"),
                    ("PULSE_SERVER", "none"), ("LANG", "de_DE.UTF-8"), ("WAYLAND_DISPLAY", "wayland-bits-0"), ("LD_LIBRARY_PATH", "(unset)"),
                    ("XDG_CONFIG_HOME", f"{H}/.local/share/brave-bin/config")]:
        need(v.get(f"env {k}") == want, f"environment {k}={want}", v.get(f"env {k}", "-"))
    need((v.get("sandbox") or "").startswith("ok "), "Chromium's namespace sandbox calls work inside the view (nested user namespace, PID, network, chroot)", v.get("sandbox", "-"))
    need(v.get("bookmarks") == "stick-bookmarks" and "event=pkg.run.profile.taken" in err, "the stick's profile is taken over at the first start", f"{v.get('bookmarks')}\n{err}")
    st2, o2, _ = run(c, [BB, "ls", "-A", STICK])
    need(ok(st2) and o2.split() == [".moved-to"], "the stick's folder keeps only the note where its profile went", o2)
    st2, o2, _ = run(c, [BB, "stat", "-c", "%a %u", PROFILE])
    need(ok(st2) and o2.strip() == "700 1000", "the profile is uid 1000's own, 0700", o2)
    st, out, err = run(c, lib + ["brave-bin"], ENV)
    need(ok(st) and values(out).get("bookmarks") == "stick-bookmarks" and "profile.taken" not in err, "a later start takes nothing over and keeps the profile", f"{out}{err}")
    # Two starts, two views (each with its own /tmp): the second reaches the first through TMPDIR.
    st, out, err = sh(c, f"{BR} --store {R} brave-bin --hold > {W}/first.txt 2>&1 & sleep 3; {BR} --store {R} brave-bin https://b.test/; r=$?; wait; cat {W}/first.txt; exit $r", 90000)
    print(out.rstrip())
    need(ok(st) and "handover = sent" in out, "a second start hands its address over", f"{st}\n{out}{err}")
    got = next((l for l in out.splitlines() if l.startswith("received = ")), "")
    need(got.endswith(" https://b.test/") and "/run/user/1000/.org.chromium.Chromium." in out, "the first start received it through the runtime folder", out)
    st, out, err = run(c, [BP, "--root", R, "remove", "brave-bin"])
    need(ok(st), "remove brave-bin", f"{st}\n{out}{err}")
    run(c, [BB, "chmod", "-R", "u+w", W])
    run(c, [BB, "rm", "-rf", W])
    if fail:
        raise SystemExit("RED: see FAIL lines above")
    print("BR-9 Brave from the repositories on V2 (QEMU, fake browser): green")


main()
