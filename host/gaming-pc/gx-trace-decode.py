#!/usr/bin/env python3
"""GT: names the register accesses of an RX 9070 XT register trace (host/gaming-pc/gpu-messung.sh's
trace-load.txt.zst) with Linux 7.3.1's header names, from the card's discovery bases in rdna4-core's ipfacts.rs.

  gx-trace-decode.py all  <trace.txt[.zst]> <out.txt>   every access and event, one line each
  gx-trace-decode.py init <trace.txt[.zst]> <out.txt>   only the task that bound the card (Linux's device init)

A line is `time task R|W BLOCK:NAME(dword) value`. VRAM reads through MM_INDEX/MM_DATA are folded into one line per
run, a repeated line (a poll) into the line and `... xN more`. The display core's own events repeat the device
accessor's and are left out. Reads the reference tree and the trace only. The init output is what gx-trace-seq.py cuts
into the fixtures of rdna4::linuxseq."""
import collections, os, re, subprocess, sys

V2 = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
# The reference tree is not in git: $LINUX_REF, the checkout's own, or the worktrees' shared one (as host/gen/rdna4).
LINUX = next((d for d in (os.environ.get("LINUX_REF"), V2 + "/ref/linux-7.3.1", "/var/tmp/claude-v2/ref/linux-7.3.1") if d and os.path.isdir(d)), None)
if LINUX is None:
    raise SystemExit("gx-trace-decode: no Linux 7.3.1 reference tree (set LINUX_REF)")
ASIC = LINUX + "/drivers/gpu/drm/amd/include/asic_reg"


def bases():
    src = open(V2 + "/crates/drivers/rdna4-core/src/ipfacts.rs").read()
    hw = dict((n, int(v)) for n, v in re.findall(r"pub const (\w+): u16 = (\d+);", src))
    tab = src[src.index("pub static TABLE"):]
    out = {}
    for m in re.finditer(r"e\(\s*(hw::\w+|\d+),\s*(\d+),\s*\((\d+), (\d+), (\d+)\),\s*&\[([^\]]*)\]", tab):
        name = m.group(1)
        hid = hw[name[4:]] if name.startswith("hw::") else int(name)
        b = [int(x.replace("_", ""), 0) for x in re.findall(r"0x[0-9a-fA-F_]+|\d+", m.group(6))]
        out.setdefault((hid, int(m.group(2))), b)
    return hw, out

HW, BASES = bases()
BLOCKS = [  # header, hw id, instance, tag
    ("gc/gc_12_0_0_offset.h", "GC", 0, "GC"),
    ("mmhub/mmhub_4_1_0_offset.h", "MMHUB", 0, "MMHUB"),
    ("athub/athub_4_1_0_offset.h", "ATHUB", 0, "ATHUB"),
    ("oss/osssys_7_0_0_offset.h", "OSSSYS", 0, "OSS"),
    ("hdp/hdp_7_0_0_offset.h", "HDP", 0, "HDP"),
    ("nbif/nbif_6_3_1_offset.h", "NBIF", 0, "NBIF"),
    ("mp/mp_14_0_2_offset.h", "MP0", 0, "MP"),
    ("smuio/smuio_14_0_2_offset.h", "SMUIO", 0, "SMUIO"),
    ("thm/thm_14_0_2_offset.h", "THM", 0, "THM"),
    ("dcn/dcn_4_1_0_offset.h", "DMU", 0, "DCN"),
    ("vcn/vcn_5_0_0_offset.h", "UVD", 0, "VCN"),
    ("lsdma/lsdma_7_0_0_offset.h", "LSDMA", 0, "LSDMA"),
]

def names():
    m = collections.defaultdict(list)
    for hdr, hwname, inst, tag in BLOCKS:
        b = BASES.get((HW[hwname], inst))
        if b is None:
            print("no bases for", hwname, file=sys.stderr); continue
        text = open(ASIC + "/" + hdr).read()
        offs = dict(re.findall(r"#define\s+(?:reg|mm)(\w+?)\s+(0x[0-9a-fA-F]+)\s", text))
        idx = dict(re.findall(r"#define\s+(?:reg|mm)(\w+?)_BASE_IDX\s+(\d+)", text))
        for n, o in offs.items():
            if n.endswith("_BASE_IDX") or n not in idx: continue
            i = int(idx[n])
            if i >= len(b): continue
            m[b[i] + int(o, 16)].append(tag + ":" + n)
    return m

NAMES = names()
def name(dw):
    n = NAMES.get(dw)
    if n: return n[0] if len(n) == 1 else n[0] + "|+%d" % (len(n) - 1)
    return "?"

LINE = re.compile(r"^\s*(.+?)-(\d+)\s+\[(\d+)\]\s+\S+\s+([\d.]+): (\w+): (.*)$")


def lines(path):
    if path.endswith(".zst"):
        p = subprocess.Popen(["zstd", "-dc", path], stdout=subprocess.PIPE, text=True, errors="replace")
        yield from p.stdout
        if p.wait() != 0:
            raise SystemExit("zstd could not read " + path)
    else:
        yield from open(path, errors="replace")


class Out:
    """Folds a line that repeats (time aside) into a count."""

    def __init__(self, f):
        self.f, self.prev, self.n = f, None, 0

    def put(self, t, rest):
        if rest == self.prev:
            self.n += 1
            return
        self.end()
        self.f.write(t + " " + rest + "\n")
        self.prev = rest

    def end(self):
        if self.n:
            self.f.write("      ... x%d more\n" % self.n)
        self.n = 0


def decode(src, dst, only_init):
    out = Out(open(dst, "w"))
    run = None  # a folded VRAM read: [time, first address, dwords, task]
    hi = idx = 0
    binder = None

    def flush():
        nonlocal run
        if run:
            out.put(run[0], "%s VRAM-READ 0x%x..+%d dwords" % (run[3], run[1], run[2]))
            run = None

    for l in lines(src):
        m = LINE.match(l)
        if not m:
            continue
        task, pid, _cpu, t, ev, rest = m.groups()
        who = task + "-" + pid
        if binder is None and ev.startswith("amdgpu_device_"):
            binder = who
        if only_init and who != binder:
            continue
        if ev in ("amdgpu_device_rreg", "amdgpu_device_wreg"):
            _, reg, val = [int(x, 16) for x in rest.split(", ")]
            w = ev.endswith("wreg")
            if reg == 6 and w:
                hi = val
                continue
            if reg == 0 and w:
                idx = val
                continue
            if reg == 1 and (idx & 0x80000000):
                addr = (hi << 31) | (idx & 0x7FFFFFFF)
                if not w:
                    if run and addr == run[1] + 4 * run[2]:
                        run[2] += 1
                    else:
                        flush()
                        run = [t, addr, 1, task]
                    continue
                flush()
                out.put(t, "%s VRAM-WRITE 0x%x = 0x%08x" % (task, addr, val))
                continue
            flush()
            if reg == 1:
                out.put(t, "%s %s MM_DATA[index 0x%x] 0x%08x" % (task, "W" if w else "R", idx, val))
            else:
                out.put(t, "%s %s %s(0x%x) 0x%08x" % (task, "W" if w else "R", name(reg), reg, val))
        elif ev in ("amdgpu_dc_rreg", "amdgpu_dc_wreg"):
            continue
        else:
            flush()
            out.put(t, "%s EV %s %s" % (task, ev, rest[:160]))
    flush()
    out.end()


if __name__ == "__main__":
    if len(sys.argv) != 4 or sys.argv[1] not in ("all", "init"):
        raise SystemExit(__doc__)
    decode(sys.argv[2], sys.argv[3], sys.argv[1] == "init")
