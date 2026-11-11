#!/usr/bin/env python3
"""GM-1d: reads vulkaninfo's and deqp-vk's output and compares them with a reference (host/tests/gm/ref-780m/).

  vkdiff.py extract <vulkaninfo.out>        the reference form of a full vulkaninfo run (vulkan.ref)
  vkdiff.py compare <vulkaninfo.out> <ref> <allow>   one line per difference; exit 1 on one that is not allowed
  vkdiff.py deqp-ref <deqp-vk stdout>       the reference form of a deqp-vk run: one "case status" line per case
  vkdiff.py deqp <deqp-vk stdout> <ref>     one line per difference; exit 1 on any

`vulkaninfo --summary` has only the device's identity; heaps, queue families and extensions need the full run, so a
reference holds both: its identity lines are compared with either form."""
import fnmatch
import re
import sys

# What says which device and driver this is. Versions (driverInfo, apiVersion, driverVersion) are not among them: the
# stage's Mesa moves on, the capture does not.
IDENTITY = ("deviceName", "vendorID", "deviceID", "deviceType", "driverID", "driverName")
# A heap's size follows the carve-out and the RAM, so its size only has to be of the same order.
HEAP_RATIO = 2.0
OK_STATUS = ("Pass", "QualityWarning", "CompatibilityWarning")


def devices(text):
    """The number of devices the run lists (GPU0:, GPU1:, ...)."""
    return len(re.findall(r"^GPU\d+:\s*$", text, re.M))


def identity(text):
    """The identity keys, from `--summary`'s device block or the full run's property blocks (first device)."""
    head = re.split(r"^GPU1:\s*$", text, flags=re.M)[0]
    out = {}
    for k in IDENTITY:
        m = re.search(rf"^\s+{k}\s*= (.*)$", head, re.M)
        if m:
            out[k] = m.group(1).strip()
    return out


def parse_full(text):
    """{identity, heaps, types, queues, extensions} of a full vulkaninfo run; a part the run lacks stays empty."""
    head = re.split(r"^GPU1:\s*$", text, flags=re.M)[0]
    heaps = []
    for m in re.finditer(r"^\s+memoryHeaps\[(\d+)\]:\n\s+size\s*= (\d+)[^\n]*\n(?:\s+budget[^\n]*\n)?(?:\s+usage[^\n]*\n)?\s+flags:[^\n]*\n((?:\s+\w+\n)*)", head, re.M):
        flags = " ".join(sorted(x for x in m.group(3).split() if x != "None"))
        heaps.append((int(m.group(2)), flags or "none"))
    types = [(int(m.group(1)), int(m.group(2), 16)) for m in re.finditer(r"^\s+memoryTypes\[\d+\]:\n\s+heapIndex\s*= (\d+)\n\s+propertyFlags\s*= (0x[0-9a-fA-F]+)", head, re.M)]
    queues = []
    for m in re.finditer(r"^\s+queueProperties\[\d+\]:\n.*?queueCount\s*= (\d+)\n\s+queueFlags\s*= ([^\n]*)\n\s+timestampValidBits\s*= (\d+)", head, re.M | re.S):
        flags = " ".join(sorted(x for x in m.group(2).replace("|", " ").split()))
        queues.append((flags, int(m.group(1)), int(m.group(3))))
    ext = {}
    m = re.search(r"^Device Extensions: count = \d+\n(.*?)(?=^\S|\Z)", head, re.M | re.S)
    if m:
        for name, rev in re.findall(r"^\s+(VK_\w+)\s*: extension revision (\d+)", m.group(1), re.M):
            ext[name] = int(rev)
    return {"identity": identity(text), "heaps": heaps, "types": types, "queues": queues, "extensions": ext}


def write_ref(p):
    out = [f"identity {k} = {p['identity'][k]}" for k in IDENTITY if k in p["identity"]]
    out += [f"heap {i} size={s} flags={f}" for i, (s, f) in enumerate(p["heaps"])]
    out += [f"type {i} heap={h} flags=0x{f:04x}" for i, (h, f) in enumerate(p["types"])]
    out += [f"queue {f.replace(' ', ',')} count={c} ts={t}" for f, c, t in p["queues"]]
    out += [f"ext {n} {r}" for n, r in sorted(p["extensions"].items())]
    return "\n".join(out) + "\n"


def read_ref(text):
    p = {"identity": {}, "heaps": [], "types": [], "queues": [], "extensions": {}}
    for line in text.splitlines():
        if line.startswith("#") or not line.strip():
            continue
        kind, rest = line.split(" ", 1)
        if kind == "identity":
            k, v = rest.split(" = ", 1)
            p["identity"][k] = v
        elif kind == "heap":
            m = re.match(r"\d+ size=(\d+) flags=(.*)$", rest)
            p["heaps"].append((int(m.group(1)), m.group(2)))
        elif kind == "type":
            m = re.match(r"\d+ heap=(\d+) flags=(0x[0-9a-f]+)$", rest)
            p["types"].append((int(m.group(1)), int(m.group(2), 16)))
        elif kind == "queue":
            m = re.match(r"(\S+) count=(\d+) ts=(\d+)$", rest)
            p["queues"].append((m.group(1).replace(",", " "), int(m.group(2)), int(m.group(3))))
        elif kind == "ext":
            n, r = rest.split()
            p["extensions"][n] = int(r)
        else:
            raise ValueError(f"reference line {line!r}")
    return p


def read_allow(text):
    """Lines `key | here | real | why`; key, here and real may hold fnmatch patterns (VK_KHR_video_*)."""
    out = []
    for line in text.splitlines():
        if line.startswith("#") or "|" not in line:
            continue
        k, here, real, why = (x.strip() for x in line.split("|", 3))
        out.append((k, here, real, why))
    return out


def compare_identity(mine, ref):
    return [(f"identity {k}", mine.get(k, "absent"), v) for k, v in ref.items() if mine.get(k) != v]


def compare_heaps(mine, ref):
    out = []
    if len(mine) != len(ref):
        out.append(("heap count", str(len(mine)), str(len(ref))))
    for i, ((s, f), (rs, rf)) in enumerate(zip(mine, ref)):
        if f != rf:
            out.append((f"heap {i} flags", f, rf))
        if not rs / HEAP_RATIO <= s <= rs * HEAP_RATIO:
            out.append((f"heap {i} size", str(s), str(rs)))
    return out


def compare_types(mine, ref):
    """Types as (heap, flags) with their count: their order and number follow Mesa's version, what a program finds does not."""
    def counts(types):
        out = {}
        for t in types:
            out[t] = out.get(t, 0) + 1
        return out
    have, want = counts(mine), counts(ref)
    show = lambda n: str(n) if n else "absent"
    return [(f"type heap={h} flags=0x{f:04x}", show(have.get((h, f))), show(want.get((h, f)))) for h, f in sorted(set(have) | set(want)) if have.get((h, f)) != want.get((h, f))]


def compare_queues(mine, ref):
    """Families by their flags: a family that goes away must not shift the others into a difference."""
    have = {f: (c, t) for f, c, t in mine}
    want = {f: (c, t) for f, c, t in ref}
    show = lambda q: f"count={q[0]} ts={q[1]}" if q else "absent"
    return [(f"queue {f}", show(have.get(f)), show(want.get(f))) for f in sorted(set(have) | set(want)) if have.get(f) != want.get(f)]


def compare_extensions(mine, ref):
    show = lambda r: "absent" if r is None else str(r)
    return [(f"ext {n}", show(mine.get(n)), show(ref.get(n))) for n in sorted(set(mine) | set(ref)) if mine.get(n) != ref.get(n)]


def split_allowed(diffs, allow):
    """(differences not allowed, allowed ones with their reason), and the allow lines no difference used."""
    new, known, used = [], [], set()
    for d in diffs:
        for i, (k, here, real, why) in enumerate(allow):
            if fnmatch.fnmatchcase(d[0], k) and fnmatch.fnmatchcase(d[1], here) and fnmatch.fnmatchcase(d[2], real):
                known.append((*d, why))
                used.add(i)
                break
        else:
            new.append(d)
    # A pattern line is a class of differences (no VCN yet), which may well not be there on this run.
    return new, known, [a[0] for i, a in enumerate(allow) if i not in used and "*" not in a[0] + a[1] + a[2]]


def compare_all(mine, ref):
    """{part: differences} of a parsed full run against a parsed reference."""
    return {"identity": compare_identity(mine["identity"], ref["identity"]),
            "memory heaps": compare_heaps(mine["heaps"], ref["heaps"]),
            "memory types": compare_types(mine["types"], ref["types"]),
            "queue families": compare_queues(mine["queues"], ref["queues"]),
            "extensions": compare_extensions(mine["extensions"], ref["extensions"])}


def parse_deqp(text):
    """{case: status} of deqp-vk's stdout. A case with no status line after it (the program died in it) is 'Died'."""
    out, cur = {}, None
    for line in text.splitlines():
        m = re.match(r"Test case '(dEQP-[^']+)'\.\.", line)
        if m:
            if cur:
                out[cur] = "Died"
            cur = m.group(1)
            continue
        m = re.match(r"\s+(Pass|Fail|NotSupported|QualityWarning|CompatibilityWarning|InternalError|ResourceError|DeviceLost|Crash|Timeout|Waiver)\b", line)
        if m and cur:
            out[cur] = m.group(1)
            cur = None
    if cur:
        out[cur] = "Died"
    return out


def read_deqp_ref(text):
    out = {}
    for line in text.splitlines():
        if line.startswith("#") or not line.strip():
            continue
        case, status = line.split()
        out[case] = status
    return out


def compare_deqp(mine, ref):
    """(case, here, reference) of every case whose result is not the reference's; a warning counts as its Pass."""
    def same(here, want):
        return here == want or (want in OK_STATUS and here in OK_STATUS)
    out = [(c, mine.get(c, "not run"), s) for c, s in sorted(ref.items()) if not same(mine.get(c), s)]
    out += [(c, s, "not in the reference") for c, s in sorted(mine.items()) if c not in ref]
    return out


def compare_deqp_support(mine, ref):
    """A proof on a node that cannot draw: only what the device says it supports can be judged, so a case may fail, but
    NotSupported must be where the reference has it (the features are the chip's) and no case may kill the program."""
    out = [(c, mine.get(c, "not run"), s) for c, s in sorted(ref.items()) if mine.get(c, "not run") in ("not run", "Died", "Crash")]
    out += [(c, mine[c], s) for c, s in sorted(ref.items()) if c in mine and (mine[c] == "NotSupported") != (s == "NotSupported")]
    return out


def group(case):
    """The step a case belongs to: its first two names after dEQP-VK (one for info)."""
    parts = case.split(".")[1:]
    return parts[0] if parts[0] == "info" else ".".join(parts[:2])


def main(argv):
    if len(argv) >= 2 and argv[0] == "extract":
        print(write_ref(parse_full(open(argv[1], errors="replace").read())), end="")
        return 0
    if len(argv) == 4 and argv[0] == "compare":
        mine = parse_full(open(argv[1], errors="replace").read())
        ref = read_ref(open(argv[2]).read())
        allow = read_allow(open(argv[3]).read())
        bad = 0
        for part, diffs in compare_all(mine, ref).items():
            new, known, _ = split_allowed(diffs, allow)
            bad += len(new)
            print(f"{part}: {len(new)} differences, {len(known)} known")
            for k, here, real in new:
                print(f"  {k}: here {here}, reference {real}")
        return 1 if bad else 0
    if len(argv) == 2 and argv[0] == "deqp-ref":
        for c, s in sorted(parse_deqp(open(argv[1], errors="replace").read()).items()):
            print(c, s)
        return 0
    if len(argv) == 3 and argv[0] == "deqp":
        diffs = compare_deqp(parse_deqp(open(argv[1], errors="replace").read()), read_deqp_ref(open(argv[2]).read()))
        for c, here, want in diffs:
            print(f"{c}: here {here}, reference {want}")
        return 1 if diffs else 0
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
