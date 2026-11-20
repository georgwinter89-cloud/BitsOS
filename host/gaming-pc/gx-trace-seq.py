#!/usr/bin/env python3
"""GX-10: cuts Linux's device init of the RX 9070 XT (the `init` output of gx-trace-decode.py) into named sections in
Linux's order and writes one fixture per section, which `rdna4::linuxseq` compares a bring-up stage's register
accesses with.

  gx-trace-seq.py <init.txt[.zst]> <dir>     writes <dir>/<section>.txt for every section

A fixture line is `<init line> <kind> ...`:
  W    NAME 0xdword 0xvalue                          one write
  R    NAME 0xdword 0xvalue [xN]                     one read (xN: N reads in a row with this value, no wait rule)
  WAIT NAME 0xdword mask=0x.. is=0x.. max=N last=0x.. reads=N us=N seen=VxN,..   reads until (value & mask) == is
  WAIT NAME 0xdword changed from=0x.. max=N last=0x.. reads=N us=N seen=VxN,..   reads until value != from
                                                     (max: the reads after which Linux gives up, 1 us apart)
  SLEEP us=N gap=N                                   Linux sleeps N us after the write before it (the trace shows
                                                     `gap` us without an access)
  HDP  0xdword MEMNAME 0xdword 0xvalue               Linux's HDP flush: 0 into the remapped flush register, then
                                                     the memory size read (amdgpu_hdp.c:51-60)
  MEM  0xoffset N                                    N dwords of VRAM read through MM_INDEX/MM_DATA

The trace holds no wait conditions: a wait is a read (or a run of reads) of a register named in WAITS, with Linux's
condition and bound from the source place named there; the run's values must fit both. It holds no sleeps either: a
sleep is a write named in SLEEPS, and the trace must be silent after it for at least that long. In a section a stage
is compared with, any other millisecond without an access must be named in QUIET (behind a write) or QUIET_READS
(behind a run of reads that follows a named write). A wait rule may hold in some sections only (WAIT_ONLY_IN). Sections are cut by register
patterns (the list in `cut`), never by time. Anything that does not fit stops the run with one plain line and writes
nothing."""
import hashlib, os, re, subprocess, sys

V2 = os.path.realpath(os.path.join(os.path.dirname(os.path.realpath(__file__)), "..", ".."))
ME = "host/gaming-pc/gx-trace-seq.py"
OUT_DIR = "crates/drivers/amdgpu/src/rdna4/linuxseq"

# Dwords gx-trace-decode.py leaves unnamed (its BLOCKS has no DF): include/asic_reg/df/df_4_15_offset.h:25-26,
# base index 4 of DF in the card's table (rdna4-core ipfacts.rs).
UNNAMED = {0x12400901: "NCSConfigurationRegister1"}

# Linux gives a wait up after `adev->usec_timeout` reads with udelay(1) between them (AMDGPU_MAX_USEC_TIMEOUT,
# amdgpu.h:274; psp_wait_for, amdgpu_psp.c:621-630): a count, not a time.
POLLS = 100000
# Linux's waits by register name: (name, condition, after, most reads, what a timeout does, where). "mask" waits
# until (value & mask) == is; "changed" follows a write of the register in `after`: the first read is psp_wait_for's
# argument (a plain read), the rest wait until the value differs from it. "ack" waits for the VMID's bit of the
# request written right before (PER_VMID_INVALIDATE_REQ, the request's low 16 bits: one bit, `1 << vmid`). "nonzero"
# waits until the value is not 0 and has no read before it (the fixture line is `changed from=0x00000000`).
WAITS = [
    # psp_v14_0_wait_for_bootloader runs the wait ten times (psp_v14_0.c:113-121): on the bus one run of reads.
    (r"MPASP_SMN_C2PMSG_35", ("mask", 0x80000000, 0x80000000), None, 10 * POLLS, "fails", "psp_v14_0.c:113-121"),
    (r"MPASP_SMN_C2PMSG_64", ("mask", 0x8000FFFF, 0x80000000), None, POLLS, "fails", "psp_v14_0.c:303-305 and :329-331"),
    (r"MPASP_SMN_C2PMSG_81", ("changed",), "MPASP_SMN_C2PMSG_35", POLLS, "fails", "psp_v14_0.c:230-235"),
    (r"MMVM_INVALIDATE_ENG17_SEM", ("mask", 1, 1), None, POLLS, "logs it and goes on", "gmc_v12_0.c:238-250"),
    (r"(MM|GC)VM_INVALIDATE_ENG17_ACK", ("ack",), None, POLLS, "logs it and goes on", "gmc_v12_0.c:255-263 and :289-292"),
    # __smu_msg_v1_poll_stat, before a message (but the first) and after it: `default_timeout` reads, which is
    # usec_timeout * 20 (smu_v14_0_2_ppt.c:2154).
    (r"MP1_SMN_C2PMSG_90", ("nonzero",), None, 20 * POLLS, "sends nothing more (before a message) or fails the message (after it)", "smu_cmn.c:294-309, :514-522 and :532-533"),
    # gfx_v12_0_cp_gfx_enable's loop after the unhalt: CP_STAT until 0; a timeout is a DRM_ERROR and `return 0`.
    (r"CP_STAT", ("mask", 0xFFFFFFFF, 0), None, POLLS, "logs it and goes on", "gfx_v12_0.c:2371-2382"),
    # gfx_v12_0_ring_test_ring: the scratch register until the ring's SET_UCONFIG_REG wrote it; a timeout is
    # -ETIMEDOUT, and hw_init returns it.
    (r"SCRATCH_REG0", ("mask", 0xFFFFFFFF, 0xDEADBEEF), None, POLLS, "fails the ring test (-ETIMEDOUT)", "gfx_v12_0.c:477-486"),
]
# A wait rule that holds in these sections only: the same register read elsewhere stays plain reads until the
# wave that consumes that section adds it here.
# CP_STAT in gc-boot is half of the autoload's pair poll (two plain reads, `Rule::PairPoll`), not this wait.
WAIT_ONLY_IN = {r"MP1_SMN_C2PMSG_90": ("smu",), r"CP_STAT": ("gc-cp",), r"SCRATCH_REG0": ("gc-ring",)}
# Linux's sleeps of a millisecond or more: (register written before it, the value or None for any, us, where).
SLEEPS = [
    (r"MPASP_SMN_C2PMSG_35", 0x00020000, 20000, "mdelay(20) after PSP_BL__LOAD_SOSDRV, psp_v14_0.c:229-230"),
    (r"MPASP_SMN_C2PMSG_64", None, 20000, "mdelay(20) after a ring command, psp_v14_0.c:328-329 (create) and :259-260 (destroy)"),
]
# A millisecond or more without a register access that is no sleep of Linux's: (register written before it, why).
QUIET = [
    (r"MPASP_SMN_C2PMSG_67", "Linux polls the frame's fence in memory, amdgpu_psp.c:751-764"),
]
# The same after a run of reads: (register read, (register, value) of the last write before the run, section, why).
QUIET_READS = [
    (r"MP1_SMN_C2PMSG_90", ("MP1_SMN_C2PMSG_66", 0x12), "smu", "Linux copies the table out of VRAM after TransferTableSmu2Dram, smu_cmn.c:1152-1155"),
]
GAP_US = 1000
# Sections this wave's stages are compared with: a run of equal reads there must have a wait rule.
CONSUMED = ("nbif", "mmhub", "ih", "psp-boot", "psp-ring", "smu", "gc-boot", "gc-hub", "gc-cp", "mes", "gc-ring", "sdma")


def stop(why):
    raise SystemExit("gx-trace-seq: " + why)


def text(path):
    if path.endswith(".zst"):
        p = subprocess.run(["zstd", "-dc", path], stdout=subprocess.PIPE)
        if p.returncode != 0:
            stop("zstd could not read " + path)
        return p.stdout.decode()
    return open(path).read()


ACCESS = re.compile(r"^(\d+)\.(\d{6}) \S+ ([RW]) (?:(\w+):)?([\w?]+)(?:\|\+\d+)?\(0x([0-9a-f]+)\) 0x([0-9a-f]{8})$")
VRAM = re.compile(r"^(\d+)\.(\d{6}) \S+ VRAM-READ 0x([0-9a-f]+)\.\.\+(\d+) dwords$")
MORE = re.compile(r"^\s+\.\.\. x(\d+) more$")
EVENT = re.compile(r"^\d+\.\d{6} \S+ EV ")


class Acc:
    """One access line of init.txt: `n` equal accesses, the first at `us`."""

    def __init__(self, line, us, kind, block, name, dword, value):
        self.line, self.us, self.kind, self.block, self.name, self.dword, self.value, self.n = line, us, kind, block, name, dword, value, 1


def parse(src):
    out, prev = [], None
    for no, l in enumerate(src.splitlines(), 1):
        m = ACCESS.match(l)
        if m:
            s, us, kind, block, name, dw, val = m.groups()
            dw = int(dw, 16)
            if name == "?":
                name = UNNAMED.get(dw, "UNNAMED_%x" % dw)
            prev = Acc(no, int(s) * 1000000 + int(us), kind, block or "", name, dw, int(val, 16))
            out.append(prev)
            continue
        m = VRAM.match(l)
        if m:
            prev = Acc(no, int(m.group(1)) * 1000000 + int(m.group(2)), "MEM", "", "", int(m.group(3), 16), int(m.group(4)))
            out.append(prev)
            continue
        m = MORE.match(l)
        if m:
            # A repeat belongs to the access right before it; one after an event or at the start stands for
            # accesses this script cannot name.
            if prev is None:
                stop("line %d: a repeat line with no access before it" % no)
            if prev.kind == "MEM":
                stop("line %d: a repeated VRAM read is not a form this script knows" % no)
            prev.n += int(m.group(1))
            continue
        if EVENT.match(l):
            prev = None
            continue
        stop("line %d is no access, event or repeat line: %s" % (no, l[:80]))
    return out


def first(acc, start, pred):
    for i in range(start, len(acc)):
        if pred(acc[i]):
            return i
    return None


def cut(acc):
    """The index in `acc` where each section starts, in Linux's order (amdgpu_device_ip_init, amdgpu_device.c:2369)."""
    def by(what, pred):
        return (what, lambda a, s: first(a, s, pred))

    def after(what, *preds):
        def find(a, s):
            for p in preds:
                s = first(a, s, p)
                if s is None:
                    return None
                s += 1
            return s
        return (what, find)

    def mes(a, s):
        i = first(a, s, lambda x: x.name.startswith("CP_MES_"))
        # The GRBM_GFX_CNTL write that selects the MES pipe belongs to it.
        if i and a[i - 1].kind == "W" and a[i - 1].name == "GRBM_GFX_CNTL" and i - 1 >= s:
            i -= 1
        return i

    w = lambda name: (lambda x: x.kind == "W" and x.name == name)
    r = lambda name: (lambda x: x.kind == "R" and x.name == name)
    sections = [
        ("discovery", ("access", lambda a, s: 0 if a else None)),
        ("nbif", by("access of RCC_DEV0_EPF2_STRAP2 (nbif_v6_3_1.c:480)", lambda x: x.name == "RCC_DEV0_EPF2_STRAP2")),
        ("mmhub", after("write of RCC_DEV0_EPF0_RCC_DOORBELL_APER_EN (nbif_v6_3_1.c:311)", w("RCC_DEV0_EPF0_RCC_DOORBELL_APER_EN"))),
        ("sw-init", after("read-back of MMVM_L2_BANK_SELECT_RESERVED_CID2 after a TLB flush (gmc_v12_0.c:270-279)",
                          w("MMVM_INVALIDATE_ENG17_REQ"), w("MMVM_L2_BANK_SELECT_RESERVED_CID2"), r("MMVM_L2_BANK_SELECT_RESERVED_CID2"))),
        ("ih", by("access of the OSS block (ih_v7_0.c:310)", lambda x: x.block == "OSS")),
        ("psp-boot", by("access of an MPASP_SMN_C2PMSG register (psp_v14_0.c:96)", lambda x: x.name.startswith("MPASP_SMN_C2PMSG_"))),
        ("psp-ring", by("access of MPASP_SMN_C2PMSG_64 (psp_v14_0.c:303)", lambda x: x.name == "MPASP_SMN_C2PMSG_64")),
        ("smu", by("access of an MP1_SMN_C2PMSG register", lambda x: x.name.startswith("MP1_SMN_C2PMSG_"))),
        ("display", by("access of the DCN block", lambda x: x.block == "DCN")),
        ("gc-boot", by("access of the GC block (gfx_v12_0.c:3767)", lambda x: x.block == "GC")),
        ("gc-hub", by("write of GCVM_CONTEXT0_PAGE_TABLE_BASE_ADDR_LO32", w("GCVM_CONTEXT0_PAGE_TABLE_BASE_ADDR_LO32"))),
        ("gc-cp", after("read of GCVM_INVALIDATE_ENG17_ACK", r("GCVM_INVALIDATE_ENG17_ACK"))),
        ("mes", ("access of a CP_MES register", mes)),
        ("gc-ring", by("write of CP_MAX_CONTEXT", w("CP_MAX_CONTEXT"))),
        ("sdma", by("access of an SDMA0 register", lambda x: x.name.startswith("SDMA0_"))),
        # VCN's doorbell entry is the first thing vcn_v5_0_0_hw_init does (vcn_v5_0_0.c:264), before the SMU powers
        # the block up; SDMA writes no entry for its second engine (sdma_v7_0.c:556). VCN writes it once more
        # when its ring starts, which is where late init begins.
        ("vcn", by("access of GDC_S2A0_S2A_DOORBELL_ENTRY_4_CTRL (vcn_v5_0_0.c:264)", lambda x: x.name == "GDC_S2A0_S2A_DOORBELL_ENTRY_4_CTRL")),
        ("late", after("second write of GDC_S2A0_S2A_DOORBELL_ENTRY_4_CTRL", w("GDC_S2A0_S2A_DOORBELL_ENTRY_4_CTRL"), w("GDC_S2A0_S2A_DOORBELL_ENTRY_4_CTRL"))),
    ]
    starts, at = [], 0
    for name, (what, find) in sections:
        i = find(acc, at)
        if i is None:
            stop("section %s: no %s after init line %d" % (name, what, acc[at].line if starts else 0))
        if i >= len(acc):
            stop("section %s is empty: nothing follows the %s" % (name, what))
        if starts and i <= starts[-1][1]:
            stop("section %s is empty: %s begins at init line %d with the %s, where %s would begin" % (starts[-1][0], name, acc[i].line, what, starts[-1][0]))
        starts.append((name, i))
        at = i
    return starts


def cond_ok(cond, v):
    return (v & cond[1]) == cond[2] if cond[0] == "mask" else v != cond[1]


def scoped_out(table, a_name, section):
    return any(re.fullmatch(rx, a_name) and section not in only for rx, only in table.items())


def lines_of(name, acc, nxt):
    """The fixture lines of one section (`nxt`: the first access after it); (lines, wait and sleep rules used, counts)."""
    out, used, slept = [], [], []
    count = {"W": 0, "R": 0, "WAIT": 0, "HDP": 0, "MEM": 0, "SLEEP": 0}
    hole = lines_of.hole

    def put(a, kind, rest):
        count[kind] += 1
        out.append("%d %s %s" % (a.line, kind, rest))

    def rule(a, before):
        for rx, cond, after_w, most, late, where in WAITS:
            if re.fullmatch(rx, a.name) and not scoped_out(WAIT_ONLY_IN, a.name, name):
                if cond[0] == "ack":
                    bit = before.value & 0xFFFF if before is not None and before.kind == "W" and before.name == a.name[:-3] + "REQ" else 0
                    if bit == 0 or bit & (bit - 1):
                        stop("init line %d: a read of %s that does not follow a request for one VMID (%s)" % (a.line, a.name, where))
                    return ("mask", bit, bit), most, late, where
                if after_w is None or (before is not None and before.kind == "W" and before.name == after_w):
                    return cond, most, late, where
        return None

    def silence(a, k):
        """After `a` (the access at `k`): Linux's sleep, a silence QUIET names, or in a consumed section a stop."""
        then = acc[k + 1] if k + 1 < len(acc) else nxt
        gap = then.us - a.us if then is not None else None
        if a.kind == "W":
            for rx, value, us, where in SLEEPS:
                if re.fullmatch(rx, a.name) and value in (None, a.value):
                    if gap is None or gap < us:
                        stop("init line %d: Linux sleeps %d us after this write of %s (%s); the trace shows %s" % (
                            a.line, us, a.name, where, "nothing after it" if gap is None else "%d us" % gap))
                    if (a.name, us, where) not in slept:
                        slept.append((a.name, us, where))
                    put(a, "SLEEP", "us=%d gap=%d" % (us, gap))
                    return
        if gap is None or gap < GAP_US or name not in CONSUMED:
            return
        if a.kind == "W" and any(re.fullmatch(rx, a.name) for rx, _ in QUIET):
            return
        if a.kind == "R" and wrote is not None and any(re.fullmatch(rx, a.name) and (wrote.name, wrote.value) == w and name == sec for rx, w, sec, _ in QUIET_READS):
            return
        stop("init line %d: %d us without a register access after %s, and neither SLEEPS nor QUIET says why (section %s)" % (
            a.line, gap, a.name or "the VRAM read", name))

    i, before, wrote = 0, None, None
    while i < len(acc):
        a = acc[i]
        if a.kind == "W":
            wrote = a
        if a.kind == "MEM":
            put(a, "MEM", "0x%x %d" % (a.dword, a.value))
        elif a.kind == "W":
            if a.name == "BIF_BX0_REMAP_HDP_MEM_FLUSH_CNTL":
                # nbif_v6_3_1.c:97-110: the flush register is remapped to this BAR5 byte.
                hole = lines_of.hole = a.value >> 2
            if a.dword == hole and a.value == 0:
                m = acc[i + 1] if i + 1 < len(acc) else None
                if a.n != 1 or m is None or m.kind != "R" or not m.name.endswith("_CONFIG_MEMSIZE") or m.n != 1:
                    stop("init line %d: a write of the HDP flush register (dword 0x%x) without the memory size read after it" % (a.line, hole))
                put(a, "HDP", "0x%x %s 0x%x 0x%08x" % (a.dword, m.name, m.dword, m.value))
                silence(m, i + 1)
                i, before = i + 2, m
                continue
            for _ in range(a.n):
                put(a, "W", "%s 0x%x 0x%08x" % (a.name, a.dword, a.value))
        else:
            got = rule(a, before)
            if got is None:
                if a.n > 1 and name in CONSUMED:
                    stop("init line %d: %d reads of %s in a row and no wait rule for it (section %s)" % (a.line, a.n, a.name, name))
                put(a, "R", "%s 0x%x 0x%08x%s" % (a.name, a.dword, a.value, " x%d" % a.n if a.n > 1 else ""))
            else:
                # Every read of this register in a row ([access, how many of it are left]), wait by wait.
                j = i
                run = []
                while j < len(acc) and acc[j].kind == "R" and acc[j].dword == a.dword:
                    run.append([acc[j], acc[j].n])
                    j += 1
                (cond, most, late, where), k = got, 0
                if (a.name, cond, most, late, where) not in used:
                    used.append((a.name, cond, most, late, where))
                if cond[0] == "nonzero":
                    cond = ("changed", 0)
                elif cond[0] == "changed":
                    cond = ("changed", a.value)
                    put(a, "R", "%s 0x%x 0x%08x" % (a.name, a.dword, a.value))
                    run[0][1] -= 1
                while k < len(run):
                    if run[k][1] == 0:
                        k += 1
                        continue
                    start, seen, n = run[k][0], [], 0
                    while True:
                        if k == len(run):
                            stop("init line %d: the reads of %s never meet Linux's condition (%s)" % (start.line, a.name, where))
                        x, left = run[k]
                        if left == 0:
                            k += 1
                            continue
                        if cond_ok(cond, x.value):
                            run[k][1] -= 1
                            seen.append((x.value, 1))
                            n += 1
                            break
                        seen.append((x.value, left))
                        n += left
                        run[k][1] = 0
                    if n > most:
                        stop("init line %d: %d reads of %s in one wait, more than the %d after which Linux gives up (%s)" % (start.line, n, a.name, most, where))
                    how = "mask=0x%08x is=0x%08x" % (cond[1], cond[2]) if cond[0] == "mask" else "changed from=0x%08x" % cond[1]
                    put(start, "WAIT", "%s 0x%x %s max=%d last=0x%08x reads=%d us=%d seen=%s" % (
                        a.name, a.dword, how, most, x.value, n, x.us - start.us if x is not start else 0, ",".join("0x%08xx%d" % s for s in seen)))
                silence(acc[j - 1], j - 1)
                i, before = j, acc[j - 1]
                continue
        silence(a, i)
        i, before = i + 1, a
    return out, used, slept, count


lines_of.hole = None


def main(src, dst):
    raw = text(src)
    acc = parse(raw)
    if not acc:
        stop(src + " holds no register access")
    starts = cut(acc)
    # A trace inside the repository is named by its path there, any other by its file name: never a home path.
    full = os.path.realpath(src)
    label = os.path.relpath(full, V2) if full.startswith(V2 + os.sep) else os.path.basename(src)
    sha = hashlib.sha256(open(src, "rb").read()).hexdigest()
    files, said = [], []
    for n, (name, i) in enumerate(starts):
        end = starts[n + 1][1] if n + 1 < len(starts) else len(acc)
        part = acc[i:end]
        body, used, slept, count = lines_of(name, part, acc[end] if end < len(acc) else None)
        head = ["# linuxseq %s: %s sha256 %s, init lines %d-%d, by %s; regenerate: python3 %s %s %s" % (
            name, label, sha, part[0].line, part[-1].line, ME, ME, label, OUT_DIR)]
        for reg, cond, most, late, where in used:
            how = "(value & 0x%08x) == 0x%08x" % (cond[1], cond[2]) if cond[0] == "mask" else "value != 0" if cond[0] == "nonzero" else "value != the read before the wait"
            head.append("# wait %s: until %s; after %d reads 1 us apart Linux %s; %s" % (reg, how, most, late, where))
        for reg, us, where in slept:
            head.append("# sleep after a write of %s: %d us, %s" % (reg, us, where))
        files.append((name, "\n".join(head + body) + "\n"))
        said.append("%-9s init lines %5d-%5d: %4d writes, %4d reads, %3d waits, %3d hdp flushes, %d sleeps, %d memory reads" % (
            name, part[0].line, part[-1].line, count["W"], count["R"], count["WAIT"], count["HDP"], count["SLEEP"], count["MEM"]))
    # Nothing is written or said before every section stands.
    print("\n".join(said))
    os.makedirs(dst, exist_ok=True)
    for name, body in files:
        with open(os.path.join(dst, name + ".txt"), "w") as f:
            f.write(body)


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    main(sys.argv[1], sys.argv[2])
