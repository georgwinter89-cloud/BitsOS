#!/usr/bin/env python3
"""GM-1d: milestone 1 over MCP (vk.sh): vulkaninfo against the capture of a real 780M, vkcube presenting in a
bitscomp tile, the deqp-vk subset against its reference. One PASS or FAIL per step, exit 0 only when all passed; a step
that cannot do its work is a FAIL, nothing is left out at run time (Georg 30.10.).

Usage: vk_check.py <host[:port]> (--tls <key store> | --plain) --token-file <file> [--out <dir>] [--profile real|qemu]
                   [--socket NAME] [--xdg DIR] [--info-driver ICD] [--ref-dir DIR]
Everything goes through host/mcp/mcp-call.mjs, so the token stays in its file.

Profile real (the Mini-PC after GX-8b, RADV throughout):
  ktest-*    `ul.pkg.mesa` (with `ul.pkg.mesa_sha`), `ul.pkg.deqp`: the trees on the stick are the ones the list names
  tree       `mesa/run --check`: every library resolves in the tree, the render node is there
  summary    `vulkaninfo --summary` exits 0, one device, identity as in ref-780m/vulkan.ref, no llvmpipe
  identity, memory-heaps, memory-types, queue-families, extensions   of the full `vulkaninfo` against vulkan.ref;
             a difference must be a known one (ref-780m/vulkan-allow.txt, each with its reason)
  window     `vkcube --wsi wayland` on the running desktop: it names the device, gets a new focused window (its own: found
             by its pid, vkcube sets no app_id)
  presenting bitscomp's commits rise beyond those of the desktop alone; on RADV bitscomp took its dma-bufs and refused
             none
  tile-pixels  the tile's pixels change between two MCP `screen` pictures and are not one colour
  close      bitscomp closes the window (`ctl close ID`), vkcube ends with exit 0
  deqp-ended every case of the subset ended by itself, each in a process of its own; a killed or stuck one is named
  deqp-<group>  one step per group of the subset (ref-780m/deqp-subset.txt): every case as in the reference
Profile qemu (40-vk-proof.sh, a proof of this script): the same steps, with vulkaninfo on RADV against the fake node,
the window and deqp on lavapipe (the fake draws nothing), and no ktest (class Program does not run in QEMU). Added: deqp-fake,
the subset on RADV against the fake, where a case may fail but RADV's "not supported" must match deqp-780m.ref."""
import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import time
import zlib

sys.dont_write_bytecode = True
ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
sys.path.insert(0, os.path.join(ROOT, "host", "tests", "gm", "tools"))
import vkdiff  # noqa: E402

MESA = "/boot/BITSOS2/mesa"
DEQP = "/boot/BITSOS2/deqp"
WORK = "/tmp/gm-vk"
RADV_NAME = "AMD Radeon 780M Graphics (RADV PHOENIX)"
PROFILES = {
    "real": {"info": "radeon_icd", "window": "radeon_icd", "deqp": "radeon_icd", "deqp_ref": "deqp-780m.ref",
             "device": RADV_NAME, "ktest": True, "dmabuf": True},
    "qemu": {"info": "radeon_icd", "window": "lvp_icd", "deqp": "lvp_icd", "deqp_ref": "deqp-lavapipe.ref",
             "device": "llvmpipe", "ktest": False, "dmabuf": False},
}
# Bounds of a stuck program, not of the hardware (a step past one is a FAIL).
INFO_SECS = 300
WINDOW_SECS = 90
DEQP_SECS = 3600
CASE_SECS = 300
# Commits beyond the desktop's own over the watched seconds: vkcube presents at the display's rate, 60 is a third of
# what 3 s of it would give at 60 Hz. No timing is judged: lavapipe in QEMU gets far fewer, so its profile asks for 3.
WATCH_SECS = 3
MIN_COMMITS = {"real": 60, "qemu": 3}


class Unreachable(Exception):
    pass


class Check:
    def __init__(self, a):
        self.a = a
        self.pr = PROFILES[a.profile]
        self.passed, self.failed = 0, 0
        self.ref_dir = a.ref_dir or os.path.join(ROOT, "host", "tests", "gm", "ref-780m")
        os.makedirs(a.out, exist_ok=True)
        self.env = {"PATH": "/bin:/usr/bin:/boot/BITSOS2/bin", "HOME": f"{WORK}/home", "XDG_RUNTIME_DIR": a.xdg,
                    "WAYLAND_DISPLAY": a.socket}
        self.env.update(kv.split("=", 1) for kv in getattr(a, "env", None) or [])

    def step(self, name, ok, detail=""):
        if ok:
            self.passed += 1
            print(f"PASS {name}" + (f": {detail}" if detail else ""), flush=True)
        else:
            self.failed += 1
            print(f"FAIL {name}" + (f": {detail}" if detail else ""), flush=True)
        return ok

    def note(self, text):
        print(f"  {text}", flush=True)

    def mcp(self, tool, args=None, png=None, save=None, wait=None):
        """(isError, text) of a tool call; no answer at all raises Unreachable."""
        cmd = ["node", os.path.join(ROOT, "host", "mcp", "mcp-call.mjs"), self.a.target, tool, json.dumps(args or {}), "--token-file", self.a.token_file]
        if self.a.tls:
            cmd += ["--tls", self.a.tls]
        if png:
            cmd += ["--png", png]
        if save:
            cmd += ["--save", save]
        try:
            r = subprocess.run(cmd, capture_output=True, text=True, errors="replace", timeout=wait or 600)
        except subprocess.TimeoutExpired as e:
            raise Unreachable(f"{tool}: no answer within {e.timeout} s") from e
        if r.returncode not in (0, 1):
            raise Unreachable(f"{tool}: mcp-call exit {r.returncode}: {r.stderr.strip()[-300:]}")
        return r.returncode == 1, r.stdout

    def run(self, path, args, env, secs):
        """Runs a program to its end (`run`, then `output` while it is still running); (exit kind, code, text).
        A program past `secs` is stopped: (None, None, text)."""
        _, text = self.mcp("run", {"path": path, "args": args, "env": env, "timeout_ms": 60000}, wait=120)
        end = time.time() + secs
        m = re.search(r"\(call (\d+)\)", text)
        while m and not ended(text):
            if time.time() > end:
                self.mcp("stop", {"call": int(m.group(1))})
                return None, None, text
            time.sleep(2)
            _, more = self.mcp("output", {"call": int(m.group(1))})
            text += "\n" + more
        e = ended(text)
        return (*(e or (None, None)), text)

    def sh(self, cmd, secs=60):
        kind, code, text = self.run("/bin/sh", ["-c", cmd], self.env, secs)
        return kind == "exit" and code == 0, text

    def mesa_env(self, driver):
        return dict(self.env, MESA_VK_DRIVERS=driver)

    def ctl(self, *words):
        err, text = self.mcp("run", {"path": "/bin/bitscomp", "args": ["ctl", "--socket", self.a.socket, *words], "env": self.env, "timeout_ms": 20000}, wait=60)
        return text

    def save(self, name, text):
        with open(os.path.join(self.a.out, name), "w") as f:
            f.write(text)

    def ktests(self):
        # A filter is a prefix: "ul.pkg.mesa" runs ul.pkg.mesa and ul.pkg.mesa_sha.
        for name, want in (("ul.pkg.mesa", 2), ("ul.pkg.deqp", 1)):
            _, text = self.mcp("ktest", {"filter": name, "timeout_ms": 900000}, wait=1000)
            self.save(f"ktest-{name}.txt", text)
            m = re.search(r"passed=(\d+) failed=(\d+) skipped=(\d+) refused=(\d+)", text)
            good = m and int(m.group(1)) >= want and m.group(2) == m.group(3) == m.group(4) == "0"
            self.step(f"ktest-{name}", good, m.group(0) if m else text[-300:])

    def tree(self):
        kind, code, text = self.run("/bin/sh", [f"{MESA}/run", "--check"], self.mesa_env(self.pr["info"]), 300)
        self.save("check.txt", text)
        good = (kind, code) == ("exit", 0) and "resolve inside" in text and re.search(r"render nodes:.*\brenderD128\b", text) is not None
        self.step("tree", good, "libraries resolve inside the tree, renderD128 is there" if good else text[-500:])

    def info(self):
        driver = self.a.info_driver or self.pr["info"]
        ref = vkdiff.read_ref(open(os.path.join(self.ref_dir, "vulkan.ref")).read())
        allow = vkdiff.read_allow(open(os.path.join(self.ref_dir, "vulkan-allow.txt")).read())
        kind, code, text = self.run("/bin/sh", [f"{MESA}/run", "vulkaninfo", "--summary"], self.mesa_env(driver), INFO_SECS)
        self.save("vulkaninfo-summary.txt", text)
        diffs = vkdiff.compare_identity(vkdiff.identity(text), ref["identity"])
        why = []
        if (kind, code) != ("exit", 0):
            why.append(f"ended {kind} {code}")
        if vkdiff.devices(text) != 1:
            why.append(f"{vkdiff.devices(text)} devices")
        if "llvmpipe" in text:
            why.append("llvmpipe is listed: this would run on the CPU")
        why += [f"{k}: here {h}, reference {r}" for k, h, r in diffs]
        self.step("summary", not why, "; ".join(why) or vkdiff.identity(text).get("deviceName", ""))

        kind, code, text = self.run("/bin/sh", [f"{MESA}/run", "vulkaninfo"], self.mesa_env(driver), INFO_SECS)
        self.save("vulkaninfo.txt", text)
        if (kind, code) != ("exit", 0):
            self.step("vulkaninfo", False, f"ended {kind} {code}: {text[-300:]}")
            return
        mine = vkdiff.parse_full(text)
        parts = vkdiff.compare_all(mine, ref)
        counts = {"memory heaps": mine["heaps"], "memory types": mine["types"], "queue families": mine["queues"], "extensions": mine["extensions"]}
        for part, found in parts.items():
            new, known, _ = vkdiff.split_allowed(found, allow)
            have = counts.get(part, True)
            detail = f"{len(have)} compared, " if part in counts else ""
            detail += f"{len(known)} known differences"
            if new:
                detail += f"; {len(new)} new: " + "; ".join(f"{k}: here {h}, reference {r}" for k, h, r in new)
            if not have:
                detail += "; the run lists none"
            self.step(part.replace(" ", "-"), not new and bool(have), detail)
            for k, h, r, w in known:
                self.note(f"known: {k}: here {h}, reference {r} ({w})")
        for k in vkdiff.split_allowed([d for found in parts.values() for d in found], allow)[2]:
            self.note(f"note: the known difference {k} is gone: its vulkan-allow.txt line can go")

    def window(self):
        driver = self.pr["window"]
        before = self.windows()
        s0 = stats(self.ctl("stats"))
        self.note(f"bitscomp: build={s0.get('build')} renderer={s0.get('renderer')} output={s0.get('output')}")
        if not s0.get("commits_seen"):
            self.step("window", False, "bitscomp does not answer `ctl stats` on this socket: is the desktop running?")
            return
        time.sleep(WATCH_SECS)
        idle = stats(self.ctl("stats"))["commits"] - s0["commits"]
        _, text = self.mcp("run", {"path": "/bin/sh", "args": [f"{MESA}/run", "vkcube", "--wsi", "wayland"], "env": self.mesa_env(driver), "timeout_ms": 1500}, wait=60)
        m = re.search(r"\(call (\d+)\)", text)
        if not m:
            self.step("window", False, f"vkcube ended at once: {text[-500:]}")
            return
        self.vkcube, self.vkcube_text = int(m.group(1)), text
        pid = re.search(r"pid (\d+) \S+ is running", text)
        self.vkcube_pid = int(pid.group(1)) if pid else None
        try:
            self.watch(before, idle, s0)
        finally:
            if not ended(self.vkcube_output()):
                self.mcp("stop", {"call": self.vkcube})
            self.save("vkcube.txt", self.vkcube_text)

    def windows(self):
        return {int(m.group(1)) for m in re.finditer(r" id=(\d+) app_id=", self.ctl("list"))}

    def vkcube_output(self):
        """All vkcube said so far (`output` hands out only what is new)."""
        self.vkcube_text += "\n" + self.mcp("output", {"call": self.vkcube})[1]
        return self.vkcube_text

    def watch(self, before, idle, s0):
        line, end = None, time.time() + WINDOW_SECS
        while time.time() < end and not line:
            text = self.vkcube_output()
            if ended(text):
                self.step("window", False, f"vkcube ended before its window: {text[-800:]}")
                return
            line = find_window(self.ctl("list"), before, self.vkcube_pid)
            time.sleep(1)
        if not line:
            s = stats(self.ctl("stats"))
            self.step("window", False, f"no window of vkcube (pid {self.vkcube_pid}) within {WINDOW_SECS} s; bitscomp renderer={s.get('renderer')} took {s['dmabuf_imported'] - s0['dmabuf_imported']} "
                      f"dma-bufs, refused {s['dmabuf_failed'] - s0['dmabuf_failed']} (refused: CR-GM1c-2, a renderer that cannot read RADV's VRAM images); {text[-800:]}\n{self.ctl('list')}")
            return
        sel = re.search(r"Selected GPU \d+: ([^,\n]*)", self.vkcube_output())
        device = sel.group(1).strip() if sel else None
        focused = "focused=true" in line
        # vkcube names the device it chose; that it is not the CPU is what this whole run is about.
        good = device is not None and self.pr["device"] in device and focused
        self.step("window", good, f"device {device}, focused={focused}: {line}")
        s1 = stats(self.ctl("stats"))
        time.sleep(WATCH_SECS)
        s2 = stats(self.ctl("stats"))
        commits = s2["commits"] - s1["commits"]
        need = idle + MIN_COMMITS[self.a.profile]
        detail = f"{commits} commits in {WATCH_SECS} s (desktop alone {idle}, needed {need}), frames {s2['frames'] - s1['frames']}"
        good = commits >= need
        if self.pr["dmabuf"]:
            imported, failed = s2["dmabuf_imported"] - s0["dmabuf_imported"], s2["dmabuf_failed"] - s0["dmabuf_failed"]
            detail += f", dma-bufs taken {imported}, refused {failed}"
            good = good and imported >= 1 and failed == 0
            if failed:
                detail += f" (a renderer that cannot read RADV's VRAM images: CR-GM1c-2; renderer={s2.get('renderer')})"
        self.step("presenting", good, detail)
        self.pixels(line)
        self.close(line)

    def pixels(self, line):
        m = re.search(r"frame=(-?\d+),(-?\d+) (\d+)x(\d+)", line)
        if not m:
            self.step("tile-pixels", False, f"no frame in {line}")
            return
        rect = tuple(int(x) for x in m.groups())
        seen = []
        for i in range(2):
            path = os.path.join(self.a.out, f"vkcube-{i}.png")
            err, text = self.mcp("screen", {}, png=path)
            if err or not os.path.exists(path):
                self.step("tile-pixels", False, f"MCP screen: {text[-300:]}")
                return
            seen.append(crop_hash(path, rect))
            time.sleep(1)
        (h0, colours0), (h1, _) = seen
        good = h0 != h1 and colours0 > 1
        self.step("tile-pixels", good, f"tile {rect[2]}x{rect[3]}: hash {'changes' if h0 != h1 else 'stays the same'}, {colours0} colours")

    def close(self, line):
        # By id: nobody else's window is closed, whatever has the focus now (CR-GM1d-5).
        wid = re.search(r" id=(\d+) app_id=", line).group(1)
        self.ctl(f"close {wid}")
        end = time.time() + 30
        while time.time() < end and not ended(self.vkcube_text):
            self.vkcube_output()
            time.sleep(1)
        e = ended(self.vkcube_text)
        gone = not re.search(rf" id={wid} app_id=", self.ctl("list"))
        self.step("close", e == ("exit", 0) and gone, f"vkcube ended {e}, window gone: {gone}")

    def run_cases(self, driver, tag):
        """The tree's subset on `driver`, each case in a process of its own: a fault or a lost device (the first program
        ever to submit to this chip) then costs one case, not the rest, and is named. ({case: status}, {case: how it
        ended, if not by itself}); (None, None) if an output cannot be read. deqp-vk exits 1 on a failed case, which its
        status line says; past DEQP_SECS the remaining cases stay `not run`."""
        out = f"{WORK}/{tag}.out"
        local, cases_file = os.path.join(self.a.out, f"{tag}.out"), os.path.join(self.a.out, "subset.txt")
        for f in (local, cases_file):
            if os.path.exists(f):
                os.remove(f)
        err, text = self.mcp("read_file", {"path": f"{DEQP}/subset.txt"}, save=cases_file)
        if err or not os.path.exists(cases_file):
            self.step(f"{tag}-subset", False, text[-300:])
            return None, None
        cases = open(cases_file).read().split()
        t0, troubles = time.time(), {}
        for c in cases:
            if time.time() - t0 > DEQP_SECS:
                break
            # exec: MCP's `stop` then reaches deqp-vk itself, not a shell around it.
            cmd = (f"cd {WORK} && exec /bin/sh {MESA}/run {DEQP}/deqp-vk --deqp-archive-dir={DEQP} --deqp-case={c} "
                   f"--deqp-log-filename={WORK}/{tag}.qpa --deqp-log-images=disable --deqp-log-shader-sources=disable >>{out} 2>&1")
            kind, code, _ = self.run("/bin/sh", ["-c", cmd], self.mesa_env(driver), CASE_SECS)
            if kind is None:
                troubles[c] = f"stuck, stopped after {CASE_SECS} s"
            elif (kind, code) not in (("exit", 0), ("exit", 1)):
                troubles[c] = f"{kind} {code}"
        err, text = self.mcp("read_file", {"path": out}, save=local)
        if err or not os.path.exists(local):
            self.step(f"{tag}-output", False, text[-300:])
            return None, None
        mine = vkdiff.parse_deqp(open(local, errors="replace").read())
        self.note(f"deqp-vk on {driver}: {len(mine)} of {len(cases)} cases in {time.time() - t0:.0f} s")
        return mine, troubles

    def deqp(self):
        text = open(os.path.join(self.ref_dir, self.pr["deqp_ref"])).read()
        ref = vkdiff.read_deqp_ref(text)
        if re.search(r"^# derived\b", text, re.M):
            self.note(f"{self.pr['deqp_ref']} is derived, not a Linux capture: a case that would fail on Linux too shows here as a FAIL of V2")
        mine, troubles = self.run_cases(self.pr["deqp"], "deqp")
        if mine is None:
            return
        self.step("deqp-ended", not troubles, "; ".join(f"{c}: {h}" for c, h in troubles.items()))
        diffs = vkdiff.compare_deqp(mine, ref)
        groups = {}
        for c in ref:
            groups.setdefault(vkdiff.group(c), []).append(c)
        strays = [x for x in diffs if x[0] not in ref]
        for g, cases in sorted(groups.items()):
            bad = [x for x in diffs if x[0] in cases]
            self.step(f"deqp-{g}", not bad, f"{len(cases) - len(bad)} of {len(cases)} as the reference" + "".join(f"; {c}: here {h}, reference {w}" for c, h, w in bad))
        self.step("deqp-subset-only", not strays, "".join(f"{c}: {h}; " for c, h, _ in strays))

    def deqp_fake(self):
        """The proof only: the chip's subset on RADV against the fake, which draws nothing and hangs a job on a packet it
        does not know. A case may fail, but none may kill its program or get stuck, and what RADV calls not supported must
        be what deqp-780m.ref says."""
        ref = vkdiff.read_deqp_ref(open(os.path.join(self.ref_dir, "deqp-780m.ref")).read())
        mine, troubles = self.run_cases("radeon_icd", "deqp-fake")
        if mine is None:
            return
        diffs = vkdiff.compare_deqp_support(mine, ref)
        n = {}
        for s in mine.values():
            n[s] = n.get(s, 0) + 1
        self.step("deqp-fake", not diffs and not troubles, f"RADV on the fake node: {n}, killed or stuck {troubles}" + "".join(f"; {c}: here {h}, reference {w}" for c, h, w in diffs))

    def main(self):
        steps = {"ktest": self.ktests, "tree": self.tree, "info": self.info, "window": self.window, "deqp": self.deqp, "deqp-fake": self.deqp_fake}
        try:
            good, text = self.sh(f"rm -rf {WORK}; mkdir -p {WORK}/home")
            if self.step("mcp", good, self.a.target if good else f"{self.a.target}: {text[-300:]}"):
                for name in self.a.steps.split(","):
                    try:
                        steps[name]()
                    except (OSError, ValueError, KeyError, IndexError) as e:
                        self.step(name, False, f"{type(e).__name__}: {e}")
        except Unreachable as e:
            self.step("mcp", False, str(e))
        finally:
            try:
                self.sh(f"rm -rf {WORK}")
            except Unreachable as e:
                self.note(f"{WORK} stays on the node, the next run removes it: {e}")
        print(f"vk: {self.passed} PASS, {self.failed} FAIL (output in {self.a.out})", flush=True)
        return 1 if self.failed else 0


def ended(text):
    m = re.search(r"ended: (exit|signal) (\d+)", text)
    return (m.group(1), int(m.group(2))) if m else None


def find_window(list_text, before, pid):
    """The `ctl list` line of the program's window, new since `before`; any other new window is not it. vkcube sets a title
    but no app_id; without a pid its title decides."""
    for line in list_text.splitlines():
        m = re.search(r" id=(\d+) app_id=", line)
        if not m or int(m.group(1)) in before:
            continue
        if f" pid={pid} " in line if pid is not None else ' title="vkcube" ' in line:
            return line
    return None


def stats(text):
    out = {k: int(v) for k, v in re.findall(r"\b(frames|commits|dmabuf_imported|dmabuf_failed)=(\d+)", text)}
    out["commits_seen"] = "commits" in out
    for k in ("build", "renderer", "output"):
        m = re.search(rf"\b{k}=(\S+)", text)
        out[k] = m.group(1) if m else None
    for k in ("frames", "commits", "dmabuf_imported", "dmabuf_failed"):
        out.setdefault(k, 0)
    return out


def read_png(path):
    """(width, height, rows of RGB bytes) of an 8-bit RGB or RGBA PNG, not interlaced (MCP `screen` makes RGB)."""
    data = open(path, "rb").read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError(f"{path}: not a PNG")
    at, idat, w, h, bpp = 8, [], 0, 0, 3
    while at < len(data):
        n = int.from_bytes(data[at:at + 4], "big")
        kind = data[at + 4:at + 8]
        body = data[at + 8:at + 8 + n]
        at += 12 + n
        if kind == b"IHDR":
            w, h = int.from_bytes(body[:4], "big"), int.from_bytes(body[4:8], "big")
            if body[8] != 8 or body[9] not in (2, 6) or body[12] != 0:
                raise ValueError(f"{path}: depth {body[8]}, colour type {body[9]}, interlace {body[12]}")
            bpp = 3 if body[9] == 2 else 4
        elif kind == b"IDAT":
            idat.append(body)
    raw = zlib.decompress(b"".join(idat))
    stride = w * bpp
    rows, prev = [], bytes(stride)
    for y in range(h):
        off = y * (stride + 1)
        ftype, cur = raw[off], bytearray(raw[off + 1:off + 1 + stride])
        if ftype == 1:
            for i in range(bpp, stride):
                cur[i] = (cur[i] + cur[i - bpp]) & 255
        elif ftype == 2:
            cur = bytearray((a + b) & 255 for a, b in zip(cur, prev))
        elif ftype == 3:
            for i in range(stride):
                cur[i] = (cur[i] + ((cur[i - bpp] if i >= bpp else 0) + prev[i]) // 2) & 255
        elif ftype == 4:
            for i in range(stride):
                a, b, c = cur[i - bpp] if i >= bpp else 0, prev[i], prev[i - bpp] if i >= bpp else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                cur[i] = (cur[i] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 255
        rows.append(bytes(cur))
        prev = cur
    return w, h, bpp, rows


def crop_hash(path, rect):
    """(SHA-256, number of colours) of the rectangle (x, y, w, h), clipped to the picture."""
    w, h, bpp, rows = read_png(path)
    x, y, rw, rh = rect
    x0, y0, x1, y1 = max(x, 0), max(y, 0), min(x + rw, w), min(y + rh, h)
    d = hashlib.sha256()
    colours = set()
    for row in rows[y0:y1]:
        part = row[x0 * bpp:x1 * bpp]
        d.update(part)
        colours.update(part[i:i + 3] for i in range(0, len(part), bpp * 8))
    return d.hexdigest(), len(colours)


def args():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("target")
    tls = p.add_mutually_exclusive_group(required=True)
    tls.add_argument("--tls", help="key store for mutual TLS (the Mini-PC)")
    tls.add_argument("--plain", action="store_true", help="plain HTTP (the kernel's own server, as in QEMU)")
    p.add_argument("--token-file", required=True)
    p.add_argument("--out", default=os.path.join(ROOT, "target", "minipc", "vk"))
    p.add_argument("--profile", choices=sorted(PROFILES), default="real")
    p.add_argument("--socket", default="wayland-bits-0")
    p.add_argument("--env", action="append", metavar="KEY=VALUE", help="one more variable for the programs under test")
    p.add_argument("--xdg", default="/run/user/1000")
    p.add_argument("--steps", help="comma list out of ktest,tree,info,window,deqp,deqp-fake (default all; the proof's failing runs need only some)")
    p.add_argument("--info-driver", help="vulkaninfo on this driver (the proof's check that the wrong one is caught)")
    p.add_argument("--ref-dir", help="another reference folder (the proof's check that a difference is caught)")
    a = p.parse_args()
    a.steps = a.steps or ("ktest,tree,info,window,deqp" if PROFILES[a.profile]["ktest"] else "tree,info,window,deqp,deqp-fake")
    return a


if __name__ == "__main__":
    sys.exit(Check(args()).main())
