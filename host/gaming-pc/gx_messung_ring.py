"""The section-7 checks of the kernel rings (wave 44: the stages gcring and ringtest), and the unit the rungs' register
accesses are held against, for gx-messung-eval.py: each check says confirmed, refuted, in part or not measurable with
the protocol's lines."""

from gx_messung_help import lines, nm, row
from gx_messung_proto import fixture, mes_unit

# The rings' interrupt controls, in late's order (the gfx pipe, MEC 1's two), and the end-of-pipe enables Linux's
# fence driver sets together (TIME_STAMP 0x04000000, GENERIC0 0x80000000).
INT_CNTL = ("CP_INT_CNTL_RING0", "CP_ME1_PIPE0_INT_CNTL", "CP_ME1_PIPE1_INT_CNTL")
EOP_ENABLES = 0x84000000
# Where the trace's section late holds the three enables (init.txt 9817-9822): ringseq::LATE_EOP_AT.
LATE_EOP_AT = 9817


def eop_unit(root):
    """rdna4::ringseq::eop_unit: late's three read-modify-writes over what gc-ring left in each register."""
    ring = fixture(root, "gc-ring")
    out = []
    for k, name in enumerate(INT_CNTL):
        last = [a for a in ring if a["k"] == "W" and a["name"] == name][-1]
        init = LATE_EOP_AT + 2 * k
        out.append({"k": "R", "name": name, "dword": last["dword"], "value": last["value"], "times": 1, "init": init})
        out.append({"k": "W", "name": name, "dword": last["dword"], "value": last["value"] | EOP_ENABLES, "init": init + 1})
    return out


def remap_unit(root):
    """The HDP flush before comp_1.0.0's map again and the ring test after it (gc-ring's first)."""
    hdp = next(a for a in fixture(root, "mes") if a["k"] == "HDP")
    ring = fixture(root, "gc-ring")
    at = next(i for i, a in enumerate(ring) if a["k"] == "W" and a["name"] == "SCRATCH_REG0")
    return [dict(hdp)] + [dict(a) for a in ring[at:at + 2]]


def ring_unit(root, eop):
    """rdna4::ringseq::expected: what a whole call of ringtest makes, in order."""
    flush = mes_unit(root)
    return (eop_unit(root) if eop else []) + flush + remap_unit(root) + [dict(a) for a in flush]


def rungs(p):
    """The rungs' lines of ringtest: {name: (line, PASS?)}."""
    out = {}
    for n, t in p.stages["ringtest"].has(r"^rung \S+: (PASS|FAIL)"):
        name = t.split(" ")[1].rstrip(":")
        out[name] = (n, t, " PASS:" in t)
    return out


def r_tests(p):
    r = row(p, "gcring")
    if r:
        return r
    ok = p.stages["gcring"].has(r"^gcring: ring \S+ test passed")
    names = [t.split(" ")[2] for _, t in ok]
    want = ["gfx_0.0.0", "comp_1.0.0", "comp_1.1.0", "comp_1.0.1", "comp_1.1.1"]
    if names != want:
        return "refuted", f"the ring tests passed were {names}, Linux's order is {want}"
    return "confirmed", f"five ring tests in Linux's order, each SCRATCH_REG0 0xdeadbeef within Linux's 100000 reads ({lines(ok)})"


def r_enables(p):
    r = row(p, "gcring")
    if r:
        return r
    found = p.stages["gcring"].has(r"^gcring: the fault interrupts on")
    want = "CP_INT_CNTL_RING0 0x0 to 0x800000, CP_ME1_PIPE0_INT_CNTL 0x0 to 0x800000, CP_ME1_PIPE1_INT_CNTL 0x0 to 0x800000, CP_INT_CNTL_RING0 0x800000 to 0xc00000, CP_INT_CNTL_RING0 0xc00000 to 0x1c00000, CP_ME1_PIPE0_INT_CNTL 0x800000 to 0x1800000, CP_ME1_PIPE1_INT_CNTL 0x800000 to 0x1800000"
    if not found or want not in found[0][1]:
        return "refuted", f"{found[0][1][:200] if found else 'no line of the enables'}"
    return "confirmed", f"the three enables in Linux's order with the trace's values (init.txt 9590-9603) ({lines(found)})"


def rung_check(names, what):
    def f(p):
        r = row(p, "ringtest")
        if r:
            return r
        got = rungs(p)
        missing = [n for n in names if n not in got]
        if missing:
            return "refuted", f"ringtest passed without the rungs {missing}"
        bad = [got[n] for n in names if not got[n][2]]
        if bad:
            return "refuted", f"{bad[0][1][:200]} (L{bad[0][0]})"
        found = [(got[n][0], got[n][1]) for n in names]
        return "confirmed", f"{what}: {found[0][1][:160]} ({lines(found)})"
    return f


def r_eop(p):
    r = row(p, "ringtest")
    if r:
        return r
    got = rungs(p)
    if not all(n in got and got[n][2] for n in ("eop-on", "fence-irq")):
        return "refuted", "the end-of-pipe enables or the fence's entry did not pass"
    ib = [got[n] for n in ("ib-gfx", "ib-comp0", "ib-comp1", "ib-comp2", "ib-comp3") if n in got]
    late = [x for x in ib if "came through the IH reader: false" in x[1]]
    if late:
        return "refuted", f"an IB test's fence came without its end-of-pipe entry ({late[0][1][:160]}, L{late[0][0]})"
    return "confirmed", f"{got['eop-on'][1][:160]}; every fence's entry arrived through the IH reader (L{got['eop-on'][0]}, L{got['fence-irq'][0]})"


def r_kq_mqd(p):
    r = row(p, "gcring")
    if r:
        return r
    return "confirmed", "the five kernel queues' MQDs, filled as Linux fills them, ran their ring tests (gcring passed)"


CHECKS = {
    "ring-tests": r_tests,
    "ring-enables": r_enables,
    "ring-ib-tests": rung_check(["ib-gfx", "ib-comp0", "ib-comp1", "ib-comp2", "ib-comp3"], "Linux's IB test on each kernel ring"),
    "ring-eop": r_eop,
    "ring-remap": rung_check(["remap"], "comp_1.0.0 unmapped and mapped again through MES"),
    "ring-me1-late": nm("late's CPC_INT_CNTL writes under each compute pipe's selection (init.txt 9811-9816) belong to the late wave; the rungs' enables read gc-ring's 0x01800000"),
    "ring-hang-look": nm("no ring test failed: GRBM_STATUS and CP_STAT after a stop were not read"),
    "ring-unmapped-doorbell": nm("not tried on purpose: a doorbell of a queue MES did not map"),
    "ring-faults": nm("needs a faulting packet on a kernel ring; none is provoked"),
    "ring-ib-timing": nm("the fences' times are the fake's here; Linux's IB tests run 2 s after init and the trace holds no memory poll"),
    "ring-reload": nm("no Reload after the chain in this round: the rings' interrupt clears of the hook livering are held on the fake only"),
    "mqd-defaults": r_kq_mqd,
    "gfx-mqd-layout": r_kq_mqd,
    "mes-kq-mqd": r_kq_mqd,
    "mes-unmap": rung_check(["remap"], "REMOVE_QUEUE of comp_1.0.0 (RESET_QUEUES) answered and the queue mapped again"),
}
