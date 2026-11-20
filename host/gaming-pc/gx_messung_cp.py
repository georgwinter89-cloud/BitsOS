"""The section-7 checks of the command processor's resume (gccp, wave 44), and the card's revision, for
gx-messung-eval.py."""

import re

from gx_messung_help import lines, nm, part, row


def r_cp_line(pat, also=""):
    """A row the stage's own line says: confirmed when gccp passed with it."""
    def f(p):
        r = row(p, "gccp")
        if r:
            return r
        a = p.stages["gccp"].has(pat)
        if not a:
            return "refuted", f"gccp passed without the line /{pat}/"
        return "confirmed", f"{a[0][1][:170]}{also} ({lines(a)})"
    return f


def r_constants(p):
    r = row(p, "gccp")
    if r:
        return r
    v = p.stages["gccp"].vergleich or {}
    a = p.stages["gccp"].has(r"active shader arrays 0x[0-9a-f]+, render backends")
    ok = v.get("said") == "equal" and v.get("answers") == "0"
    return ("confirmed" if ok else "refuted"), f"linuxseq {v.get('said')}, answers {v.get('answers')}; {a[0][1][:150] if a else '-'} ({lines(a)})"


def r_gc_info(p):
    r = row(p, "gccp")
    if r:
        return r
    a = p.stages["gccp"].has(r"shader engines (\d+), arrays per engine (\d+), render backends per engine (\d+), CUs per array (\d+) \(the discovery table's GC info version (\d+)\)")
    if not a:
        return "refuted", "gccp passed without its layout line"
    m = re.search(r"shader engines (\d+), arrays per engine (\d+), render backends per engine (\d+), CUs per array (\d+)", a[0][1])
    ok = m and (m.group(1), m.group(2), m.group(3), m.group(4)) == ("4", "2", "4", "8")
    return ("confirmed" if ok else "refuted"), f"{a[0][1][6:160]} (Linux's trace: 4 engines of 2 arrays, the 16 render backends and 64 CUs of the card) ({lines(a)})"


def r_cp_unhalt(p):
    r = row(p, "gccp")
    if r:
        return r
    a = p.stages["gccp"].has(r"MEC unhalted \(CP_MEC_RS64_CNTL 0x40000000 to 0x3c000000\), PFP and ME unhalted \(CP_ME_CNTL 0x1500a000 to 0x100a000")
    s = p.look("state", "gccp", r"cp registers: CP_STAT 0x0, CP_ME_CNTL 0x100a000, CP_MEC_RS64_CNTL 0x3c000000")
    if a and s:
        return "confirmed", f"{a[0][1][6:150]}; the state look after it: {s[0][1][:110]} ({lines(a + s)})"
    any_ = p.look("state", "gccp", r"cp registers")
    return "refuted", f"{a[0][1][:120] if a else 'no unhalt line with the trace values'}; {any_[0][1][:120] if any_ else 'no state look after gccp'} ({lines(a + any_)})"


def r_cp_stat(p):
    # A CP_STAT that never settles stops gccp after its WAIT line: the line judges the row, not the pass.
    if not p.ran("gccp"):
        return row(p, "gccp")
    w = p.stages["gccp"].has(r"WAIT \S+ # CP_STAT until .*: (\d+) reads in (\d+) us, last (0x[0-9a-f]+|-)")
    if not w:
        return row(p, "gccp") or ("refuted", "gccp passed without CP_STAT's WAIT line")
    m = re.search(r": (\d+) reads in (\d+) us, last (0x[0-9a-f]+|-)", w[0][1])
    reads, last = int(m.group(1)), m.group(3)
    if last == "0x00000000" and reads < 100000:
        return "confirmed", f"CP_STAT read 0 after {reads} reads in {m.group(2)} us (Linux: 21 reads in 46 us) ({lines(w)})"
    return "refuted", f"Linux's loop ran out: {w[0][1][:140]} ({lines(w)})"


def r_cp_idle(p):
    r = row(p, "gccp")
    if r:
        return r
    s = p.look("state", "gccp", r"cp registers: CP_STAT 0x0,")
    ih = p.ihs.get("gccp")
    faults = bool(ih and re.search(r"faults_(gfxhub|mmhub)=\+[1-9]", ih[1]))
    if s and not faults:
        return part(f"CP_STAT 0 in the state look after gccp and no page fault in gccp's IH counts ({lines(s)})", "the CP's own fetch counters (no register of the trace shows them)")
    return "refuted", f"{s[0][1][:100] if s else 'CP_STAT not 0 in the state look after gccp'}; faults {faults}"


def r_kiq(p):
    r = row(p, "gccp")
    if r:
        return r
    a = p.stages["gccp"].has(r"RLC_CP_SCHEDULERS (0x[0-9a-f]+) to (0x[0-9a-f]+)")
    m = re.search(r"RLC_CP_SCHEDULERS (0x[0-9a-f]+) to (0x[0-9a-f]+)", a[0][1]) if a else None
    if not m:
        return "refuted", "gccp passed without the KIQ's line"
    ok = (int(m.group(1), 16), int(m.group(2), 16)) == (0x3038, 0x30E8)
    return ("confirmed" if ok else "refuted"), f"RLC_CP_SCHEDULERS read {m.group(1)}, written {m.group(2)} (the trace: 0x3038, 0x30e8) ({lines(a)})"


def r_revision(p):
    v = p.head.get("ausfuehrung")
    if v is None:
        return "not measurable", "the protocol's head has no KOPF ausfuehrung (a run before wave 44)"
    if v == "1":
        return "confirmed", "KOPF ausfuehrung=1: STRAP0's revision as the attach read it; gcboot's golden list is revision 1's"
    return "refuted", f"KOPF ausfuehrung={v}: gcboot refuses this card before any access"


CP_CHECKS = {
    "gccp-constants": r_constants,
    "gc-info": r_gc_info,
    "csb-place": r_cp_line(r"the clear-state buffer, 75 dwords, at vram 0x[0-9a-f]+ \(mc 0x[0-9a-f]+\)", "; the Address rule held it to the VRAM manager's page"),
    "csb-flush": nm("the RLC reads the clear-state buffer only at a save-restore (gfxoff), which the run does not provoke"),
    "sh-mem-readback": nm("nothing reads SH_MEM_CONFIG or SH_MEM_BASES back, Linux neither"),
    "doorbell-entries": nm("NBIF's entries 0 and 3 show only at the first doorbell of the CP or MES (the rings' and MES's stages)"),
    "cp-unhalt": r_cp_unhalt,
    "cp-stat-wait": r_cp_stat,
    "cp-idle": r_cp_idle,
    "cp-order": nm("the model's rule (ranges before the unhalt) is Linux's order; whether the card cares is not tried"),
    "cp-reload": nm("needs a Reload after gccp; the run switches the PC off (the hook runs at the power-off, after the last line)"),
    "cp-second-fw": nm("not tried on purpose: a second fw after gccp is refused by the driver"),
    "kiq-setting": r_kiq,
    "revision": r_revision,
}
