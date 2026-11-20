"""The section-7 checks of the power controller, the graphics block and the copy engines (pmfw to sdmatest), for
gx-messung-eval.py."""

import re

from gx_messung_proto import access
from gx_messung_help import check_lines, ih_after, ih_clients, lines, part, row


MP1 = 0x1f  # SOC21_IH_CLIENTID_MP1, soc15_ih_clientid.h:108


def r_rundcbtc(p):
    r = row(p, "pmfw")
    if r:
        return r
    a = p.stages["pmfw"].has(r"RunDcBtc \(0x36, argument 0x0\): OK")
    if not a:
        return "refuted", "pmfw passed without RunDcBtc OK"
    sp = [(n, x) for n, x in ((n, access(t)) for n, t in p.stages["pmfw"].spur) if x]
    at = next((i for i, (_, x) in enumerate(sp) if x["k"] == "W" and x["name"] == "MP1_SMN_C2PMSG_66" and x["value"] == 0x36), None)
    wait = next(((n, x) for n, x in sp[at + 1:] if x["k"] == "WAIT"), None) if at is not None else None
    if wait is None:
        return "refuted", f"no WAIT after RunDcBtc's message in pmfw's trail ({lines(a)})"
    ih = ih_after(p, "pmfw")
    if not ih:
        return part(f"{a[0][1]}; its WAIT {wait[1]['reads']} reads in {wait[1]['us']} us (the trace: 6 reads)", "the IH's counts after pmfw (none recorded)")
    faults = [(n, t) for n, t in ih if re.search(r"faults_(gfxhub|mmhub)=\+[1-9]", t)]
    mp1 = [(n, t) for n, t in ih if MP1 in ih_clients(t)]
    if faults or mp1:
        return "refuted", f"after pmfw: {len(faults)} IH lines with page faults, {len(mp1)} with an MP1 interrupt ({lines(faults + mp1)})"
    return "confirmed", f"{a[0][1]} ({lines(a)}); its WAIT {wait[1]['reads']} reads in {wait[1]['us']} us (L{wait[0]}; the trace: 6 reads); from pmfw to the end no fault and no MP1 interrupt in the IH's counts ({lines(ih)})"


def r_pcie(p):
    r = row(p, "pmfw")
    if r:
        return r
    a = p.stages["pmfw"].has(r"OverridePcieParameters arguments \[406, 10406, 20406\]")
    oks = p.stages["pmfw"].has(r"OverridePcieParameters \(0x20, argument 0x[0-9a-f]+\): OK")
    if a and len(oks) == 3:
        return part(f"{a[0][1][:150]}; three OK ({lines(a + oks)})", "the `pci` tool's link speed and width of the four functions before and after, and AER errors (the run reads the link only at the attach)")
    return "refuted", f"other arguments or answers ({lines(p.stages['pmfw'].has('OverridePcie'))})"


def r_pptlimit(p):
    if not p.ran("pmfw"):
        return "not measurable", "pmfw did not run"
    sent = [(n, t) for n, t in p.stages["pmfw"].spur if re.match(r"W MP1_SMN_C2PMSG_66 \S+ 0x00000032", t)]
    if sent:
        return "refuted", f"SetPptLimit (0x32) went out ({lines(sent)})"
    lim = p.stages["pmfw"].has("socket power limit")
    return "confirmed", f"SetPptLimit never sent; the card's own limits: {lim[0][1][-60:] if lim else '-'} ({lines(lim)})"


def r_gcboot(pat):
    def f(p):
        r = row(p, "gcboot")
        if r:
            return r
        a = p.stages["gcboot"].has(pat)
        return ("confirmed", f"{a[0][1][:170]} ({lines(a)})") if a else ("refuted", f"gcboot passed without /{pat}/")
    return f


def r_bootload_value(p):
    r = row(p, "gcboot")
    if r:
        return r
    a = p.stages["gcboot"].has(r"RLC_RLCS_BOOTLOAD_STATUS (0x[0-9a-f]+)")
    m = re.search(r"RLC_RLCS_BOOTLOAD_STATUS (0x[0-9a-f]+)", a[0][1]) if a else None
    if not m:
        return "not measurable", "no round line"
    return ("confirmed" if int(m.group(1), 16) == 0x8000003F else "refuted"), f"{m.group(1)} ({lines(a)})"


def r_grbm(p):
    r = row(p, "gcboot")
    if r:
        return r
    w = [(n, t) for n, t in p.stages["gcboot"].spur if re.match(r"W GRBM_GFX_CNTL \S+ 0x00000000", t)]
    return ("confirmed", f"{len(w)} writes of 0 ({lines(w)})") if len(w) >= 3 else ("refuted", f"{len(w)} writes of 0")


def r_pulses(p):
    r = row(p, "gcboot")
    if r:
        return r
    s = p.look("state", "gcboot", r"CP_ME_CNTL 0x1500a000, CP_MEC_RS64_CNTL 0x40000000")
    any_ = p.look("state", "gcboot", r"CP_ME_CNTL")
    return ("confirmed", f"{s[0][1][:100]} ({lines(s)})") if s else ("refuted", f"{any_[0][1][:120] if any_ else 'no state look after gcboot'} ({lines(any_)})")


def r_gchub(p):
    r = row(p, "gchub")
    if r:
        return r
    v = p.stages["gchub"].vergleich
    w = p.stages["gchub"].has("GCVM_INVALIDATE_ENG17_ACK")
    return ("confirmed" if v.get("answers") == "0" else "refuted"), f"answers {v.get('answers')}; {w[0][1][:120] if w else ''} ({lines(w)})"


def r_flush_both(p):
    r = row(p, "mes")
    if r:
        return r
    v = p.stages["mes"].vergleich
    w = p.stages["mes"].has("GCVM_INVALIDATE_ENG17_ACK")
    good = v.get("said") in ("so-far", "equal")
    return ("confirmed" if good else "refuted"), f"mes {v.get('said')} under BindBothBefore GRBM_GFX_CNTL (MES's bind, the last one by registers) ({lines(w) if w else 'L' + str(v.get('at'))})"


def r_pmfw_answers(p):
    r = row(p, "pmfw")
    if r:
        return r
    v = p.stages["pmfw"].vergleich
    return ("confirmed" if v.get("answers") == "0" else "refuted"), f"pmfw's answers that are not the trace's: {v.get('answers')} (line {v.get('at')})"


def r_enable_time(p):
    if not p.ran("pmfw"):
        return "not measurable", "pmfw did not run"
    sp = [a for a in (access(t) for _, t in p.stages["pmfw"].spur) if a]
    for i, a in enumerate(sp):
        if a["k"] == "W" and a["name"] == "MP1_SMN_C2PMSG_66" and a["value"] == 0x06:
            w = next((x for x in sp[i + 1:] if x["k"] == "WAIT"), None)
            if w:
                return "confirmed", f"{w['reads']} reads in {w['us']} us (Linux 8851 reads, 16.9 ms; bound 2000000 reads)"
    return "not measurable", "EnableAllSmuFeatures did not go out"


def r_scpm(p):
    g = [(n, t) for n, t in p.gpu.get("vorher", []) if "power controller: scpm" in t]
    a = p.stages["pmfw"].has(r"scpm: Enabled") if p.ran("pmfw") else []
    if not g:
        return "not measurable", "no snapshot before the stages"
    return ("confirmed" if "Enabled" in g[0][1] else "refuted"), f"{g[0][1][:120]} ({lines(g + a)})"


def r_link(p):
    g = [(n, t) for n, t in p.gpu.get("vorher", []) if "link path" in t]
    if not g:
        return "not measurable", "no snapshot before the stages"
    ok = "generation code 4, width code 6" in g[0][1]
    if not ok:
        return "refuted", f"{g[0][1][:160]} ({lines(g)})"
    own = p.stages["pmfw"].has("link levels") if p.ran("pmfw") else []
    return part(f"{g[0][1][:160]} ({lines(g + own)})", "the `pci` tool's view of the four functions, and the link after the run (it is read only at the attach)")


def r_layout(p):
    r = row(p, "pmfw")
    if r:
        return r
    a = p.stages["pmfw"].has("own header places its parts at")
    if not a:
        return "not measurable", "no header line"
    m = re.search(r"at (OwnParts \{.*?\}); Linux reads them by its compiled header at (OwnParts \{.*?\})", a[0][1])
    rev = p.stages["pmfw"].has(r"format revision 23 \(Linux's header: 23\)")
    if m and m.group(1) == m.group(2) and rev:
        return "confirmed", f"the table's own header and Linux's compiled one agree, format revision 23 ({lines(a + rev)})"
    return "refuted", f"{a[0][1][:200]} ({lines(a)})"


def r_video(p):
    r = row(p, "pmfw")
    if r:
        return r
    a = p.stages["pmfw"].has(r"Power(Up|Down)(Vcn|Jpeg) \(0x[0-9a-f]+, argument 0x0\): OK")
    return ("confirmed" if len(a) == 4 else "refuted"), f"{len(a)} of four OK ({lines(a)})"


def r_rungs(pat, name):
    def f(p):
        if not p.ran("sdmatest"):
            return "not measurable", "sdmatest did not run"
        a = p.stages["sdmatest"].has(pat)
        bad = p.stages["sdmatest"].has(r"rung \S+: FAIL")
        if bad:
            return "refuted", f"{bad[0][1][:140]} ({lines(bad)})"
        return ("confirmed", f"{name}: {a[0][1][:150]} ({lines(a)})") if a else ("refuted", f"no line /{pat}/")
    return f


def r_fill_form(p):
    if not p.ran("sdmatest"):
        return "not measurable", "sdmatest did not run"
    a = p.stages["sdmatest"].has(r"filled as (bytes|dwords)")
    if not a:
        return "refuted", "no fill line"
    dw = [x for x in a if "filled as dwords" in x[1]]
    return ("refuted", f"filled as dwords: the model is to be corrected ({lines(dw)})") if dw else ("confirmed", f"filled as bytes ({lines(a)})")


def r_irq(p):
    if not p.ran("sdmatest"):
        return "not measurable", "sdmatest did not run"
    a = p.stages["sdmatest"].has("accesses of Linux's interrupt handlers")
    return ("confirmed", f"{lines(a)}") if a else ("not measurable", "no fault and no overflow arrived while the rungs read the IH")


def r_after_failure(pat, what):
    def f(p):
        failed = p.ran("sdmatest") and p.stages["sdmatest"].has(r"rung \S+: FAIL") or (p.ran("sdma") and not p.passed("sdma"))
        if not failed:
            return "not measurable", f"{what}: no ring test or rung failed"
        a = [(n, t) for n, l, aft, t in p.looks if re.search(pat, t)]
        return "confirmed", f"{lines(a)}"
    return f
