#!/usr/bin/env python3
"""BR-1: summarize an `strace -f -yy` trace of Brave into the tables the forecast failure list needs.

usage: strace-summary.py TRACE > summary.txt
Only the counts matter, so run-specific numbers (pids, tids, cpu numbers, temp names) are folded into
placeholders before counting.
"""
import collections
import os
import re
import sys

# The run directory of the trace (strace-brave.sh OUT_DIR): folded to $OUT / $RUN so references diff cleanly.
OUT = os.path.dirname(os.path.abspath(sys.argv[1])) if len(sys.argv) > 1 else ""

LINE = re.compile(r"^(\d+) +(.*)$")
CALL = re.compile(r"^([a-z0-9_]+)\((.*)\) += (.*)$")
RESUMED = re.compile(r"^<\.\.\. ([a-z0-9_]+) resumed>(.*)$")
ERR = re.compile(r"^-1 ([A-Z0-9]+)")


def fold(path):
    path = re.sub(r"/proc/\d+", "/proc/PID", path)
    path = re.sub(r"/task/\d+", "/task/TID", path)
    path = re.sub(r"cpu\d+", "cpuN", path)
    path = re.sub(r"/fd/\d+", "/fd/N", path)
    if OUT:
        path = path.replace(OUT + "/run", "$RUN").replace(OUT, "$OUT")
    path = re.sub(r"\.org\.chromium\.Chromium\.\w+", ".org.chromium.Chromium.X", path)
    path = re.sub(r"\.com\.brave\.Browser\.\w+", ".com.brave.Browser.X", path)
    path = re.sub(r"/[0-9a-f]{16,}(_\d+)?", "/HEX", path)
    path = re.sub(r"(Singleton\w*|\.tmp)\w*", r"\1", path)
    path = re.sub(r"/dev/disk/by-uuid/.+", "/dev/disk/by-uuid/UUID", path)
    path = re.sub(r"\d{6,}", "N", path)
    return path


BRAVE_IMAGES = ("/opt/brave-bin/brave", "/proc/self/exe", "/opt/brave-bin/chrome_crashpad_handler")
IMAGE = {}  # pid -> program it runs; children inherit at clone, execve replaces
OTHER = collections.Counter()  # calls made by non-Brave programs (xdg-settings and its tools), not counted


def calls(lines):
    for pid, name, args, ret in raw_calls(lines):
        img = IMAGE.get(pid)
        if name in ("clone", "clone3", "fork", "vfork") and ret.split(" ")[0].isdigit():
            IMAGE[ret.split(" ")[0]] = img
        if img is None and name == "execve":
            img = first_str(args)  # the first program strace starts
        if name == "execve" and outcome(ret) == "ok":
            IMAGE[pid] = first_str(args)
        if img is None or img.startswith(BRAVE_IMAGES):
            yield pid, name, args, ret
        else:
            OTHER[f"{img} {name}" if name == "<event>" else img] += 1


def raw_calls(lines):
    pending = {}
    for raw in lines:
        m = LINE.match(raw.rstrip("\n"))
        if not m:
            continue
        pid, rest = m.groups()
        if rest.startswith("---") or rest.startswith("+++"):
            yield pid, "<event>", rest, ""
            continue
        if rest.endswith("<unfinished ...>"):
            pending[pid] = rest[: -len("<unfinished ...>")].rstrip()
            continue
        r = RESUMED.match(rest)
        if r:
            rest = pending.pop(pid, r.group(1) + "(") + r.group(2)
        c = CALL.match(rest)
        if c:
            yield pid, c.group(1), c.group(2), c.group(3)


def outcome(ret):
    e = ERR.match(ret)
    if e:
        return e.group(1)
    if ret.startswith("?"):
        return "?"
    return "ok"


def first_str(args):
    m = re.search(r'"((?:[^"\\]|\\.)*)"', args)
    return m.group(1) if m else ""


def table(title, counter, limit=None):
    print(f"\n## {title}")
    for k, n in counter.most_common(limit):
        print(f"{n:7d}  {k}")


def main():
    t = {k: collections.Counter() for k in (
        "sys", "err", "ioctl", "proc", "sys_paths", "dev", "etc", "run", "other", "socket", "connect", "bind",
        "clone", "mmap", "mprotect", "madvise", "prctl", "signal", "kill", "sigaction", "exec", "fcntl",
        "sockopt", "prlimit", "sched", "futex", "memfd", "misc", "write_proc", "fs_type", "exit")}
    path_calls = {"openat", "open", "access", "faccessat2", "faccessat", "stat", "lstat", "newfstatat", "statx",
                  "readlink", "readlinkat", "mkdir", "mkdirat", "unlink", "unlinkat", "rename", "creat",
                  "inotify_add_watch", "statfs", "utimensat", "chdir", "symlink", "execve", "truncate"}
    writes = collections.defaultdict(set)
    for pid, name, args, ret in calls(open(sys.argv[1], errors="replace")):
        if name == "<event>":
            m = re.match(r"--- (SIG\w+) \{si_signo=\w+, si_code=(\w+)", args)
            if m:
                t["signal"][f"{m.group(1)} {m.group(2)}"] += 1
            elif args.startswith("+++"):
                t["exit"][re.sub(r"\d+", "N", args) if "exited" in args else args] += 1
            continue
        out = outcome(ret)
        t["sys"][name] += 1
        if out != "ok":
            t["err"][f"{name} {out}"] += 1
        a = args
        if name == "ioctl":
            fdt = re.match(r"\d+<([^>:\[]*)", a)
            req = a.split(", ")[1] if ", " in a else "?"
            req = re.sub(r"\(.*", "", req)
            t["ioctl"][f"{req:32s} on {fold(fdt.group(1)) if fdt else '?':40s} -> {out}"] += 1
        if name in path_calls:
            p = fold(first_str(a))
            if name == "statx" or name == "newfstatat" or name.endswith("at") and not p.startswith("/"):
                p = p or "(fd)"
            key = f"{p}  [{name} -> {out}]"
            bucket = ("proc" if p.startswith("/proc") else "sys_paths" if p.startswith("/sys") else
                      "dev" if p.startswith("/dev") else "etc" if p.startswith("/etc") else
                      "run" if p.startswith("/run") else "other")
            if bucket == "other":
                p2 = re.sub(r"^(/[^/]+(/[^/]+)?(/[^/]+)?).*", r"\1/...", p) if p.count("/") > 3 else p
                key = f"{p2}  [{name} -> {out}]"
            t[bucket][key] += 1
            if name == "openat" and re.search(r"O_WRONLY|O_RDWR", a) and p.startswith(("/proc", "/sys")):
                t["write_proc"][f"{p} {re.search(r'O_[A-Z_|0-9]+', a).group(0)} -> {out}"] += 1
            if name == "execve":
                t["exec"][f"{p} -> {out}"] += 1
        if name == "socket":
            t["socket"][f"{a} -> {out}"] += 1
        if name == "socketpair":
            t["socket"][f"socketpair({a.split(', [')[0]}) -> {out}"] += 1
        if name in ("connect", "bind", "sendto", "sendmsg") and ("sun_path" in a or "sin" in a or "nl_" in a):
            m = re.search(r"\{sa_family=(\w+), (sun_path=@?\"[^\"]*\"|sin6?_port=htons\(\d+\)[^}]*|nl_[^}]*)", a)
            if m and name in ("connect", "bind"):
                t[name][f"{m.group(1)} {fold(m.group(2))} -> {out}"] += 1
        if name in ("clone", "clone3"):
            m = re.search(r"flags=([A-Z_|0-9]+)", a)
            t["clone"][f"{name} {m.group(1) if m else a[:60]} -> {out}"] += 1
        if name == "mmap":
            parts = a.split(", ")
            if len(parts) >= 5:
                t["mmap"][f"prot={parts[2]} flags={parts[3]} fd={'-1' if parts[4] == '-1' else 'file'} -> {out}"] += 1
        if name in ("mprotect", "pkey_mprotect"):
            parts = a.split(", ")
            t["mprotect"][f"{name} {parts[2] if len(parts) > 2 else a} -> {out}"] += 1
        if name in ("madvise", "mremap", "mlock", "munlock", "mincore", "msync", "process_madvise"):
            parts = a.split(", ")
            t["madvise"][f"{name} {parts[-1]} -> {out}"] += 1
        if name == "prctl":
            t["prctl"][f"{re.sub(r', 0x[0-9a-f]+|, \d+', '', a.split(', \"')[0])[:70]} -> {out}"] += 1
        if name in ("kill", "tgkill", "tkill", "rt_sigqueueinfo", "rt_tgsigqueueinfo", "pidfd_send_signal"):
            sig = re.search(r"SIG\w+|, 0\)", a)
            t["kill"][f"{name} {sig.group(0) if sig else a} -> {out}"] += 1
        if name == "rt_sigaction" and ", {sa_handler=" in a:
            m = re.match(r"(SIG\w+), \{sa_handler=(\w+), sa_mask=[^,]*, sa_flags=([A-Z_|]+)", a)
            if m:
                h = m.group(2) if m.group(2) in ("SIG_DFL", "SIG_IGN") else "handler"
                t["sigaction"][f"{m.group(1)} {h} {m.group(3)}"] += 1
        if name == "fcntl":
            t["fcntl"][f"{re.sub(r'^[^,]*, ', '', a)[:50]} -> {out}"] += 1
        if name in ("setsockopt", "getsockopt"):
            parts = a.split(", ")
            t["sockopt"][f"{name} {parts[1] if len(parts) > 1 else ''} {parts[2] if len(parts) > 2 else ''} -> {out}"] += 1
        if name == "prlimit64":
            parts = a.split(", ")
            t["prlimit"][f"{parts[1] if len(parts) > 1 else a} {'set' if len(parts) > 2 and parts[2] != 'NULL' else 'get'} -> {out}"] += 1
        if name.startswith("sched_") or name in ("setpriority", "getpriority", "ioprio_set", "ioprio_get"):
            # Thread ids differ on every run: the target becomes T (0 = the caller stays 0 for *priority).
            who = re.sub(r"^(PRIO_\w+), [1-9][0-9]*", r"\1, T", re.sub(r"^[0-9]+(?=,|$)", "T", a))
            t["sched"][f"{name}({who[:60]}) -> {out}"] += 1
        if name == "futex":
            parts = a.split(", ")
            t["futex"][f"{parts[1] if len(parts) > 1 else a} -> {out}"] += 1
        if name == "memfd_create":
            t["memfd"][f"{a} -> {out}"] += 1
        if name in ("eventfd2", "epoll_create1", "pipe2", "timerfd_create", "signalfd4", "inotify_init1",
                    "inotify_init", "getrandom", "flock", "fallocate", "fadvise64", "sysinfo", "uname",
                    "seccomp", "unshare", "chroot", "capset", "capget", "landlock_create_ruleset", "pkey_alloc",
                    "rseq", "set_robust_list", "sigaltstack", "arch_prctl", "membarrier", "setsid", "getrusage",
                    "fsetxattr", "fgetxattr", "getxattr", "lgetxattr", "listxattr", "waitid", "wait4",
                    "pidfd_open", "close_range", "statfs", "fstatfs", "shutdown", "accept4", "listen",
                    "recvmmsg", "sendmmsg", "copy_file_range", "sendfile", "splice", "io_uring_setup",
                    "name_to_handle_at", "process_vm_readv", "ptrace", "personality"):
            short = re.sub(r"0x[0-9a-f]{6,}", "ADDR", re.sub(r'"[^"]*"\.*', '"…"', a))
            short = re.sub(r"\b\d{3,}\b", "N", short)
            if name in ("fstatfs", "statfs"):
                m = re.search(r"f_type=(\w+)", a)
                t["fs_type"][f"{name} {m.group(1) if m else ''} -> {out}"] += 1
                continue
            t["misc"][f"{name}({short[:70]}) -> {out}"] += 1

    table("calls of other programs (not counted above or below)", OTHER)
    table("syscalls by count", t["sys"])
    table("errors (syscall errno)", t["err"])
    for k, title in (("ioctl", "ioctl request on fd -> outcome"), ("proc", "/proc paths"), ("sys_paths", "/sys paths"),
                     ("dev", "/dev paths"), ("etc", "/etc paths"), ("run", "/run paths"),
                     ("write_proc", "/proc and /sys opened for writing"), ("other", "other paths (3 levels)"),
                     ("exec", "execve"), ("socket", "socket / socketpair"), ("connect", "connect"), ("bind", "bind"),
                     ("sockopt", "setsockopt / getsockopt"), ("clone", "clone / clone3 flags"),
                     ("mmap", "mmap prot/flags"), ("mprotect", "mprotect prot"), ("madvise", "madvise / mremap ..."),
                     ("prctl", "prctl options"), ("signal", "signals delivered"), ("kill", "signals sent"),
                     ("sigaction", "rt_sigaction installs"), ("fcntl", "fcntl"), ("prlimit", "prlimit64"),
                     ("sched", "scheduling"), ("futex", "futex ops"), ("memfd", "memfd_create"),
                     ("fs_type", "statfs f_type"), ("misc", "other calls (args folded)"), ("exit", "process ends")):
        table(title, t[k], 400 if k == "other" else None)


if __name__ == "__main__":
    main()
