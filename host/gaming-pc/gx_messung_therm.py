"""Wave 44: the section-7 rows of the card's thermal taker for gx-messung-eval.py, read from the protocol's THERMO
lines (crates/drivers/amdgpu/src/rdna4/messtherm.rs): the taker's alerts after every stage, and after pmfw and at
the end the card's limits, one read of its temperatures and the run's judgement."""

import re

from gx_messung_help import lines as _lines, nm, part, row

# The run stops within this many degrees of a limit (crates/drivers/amdgpu/src/rdna4/messtherm.rs, MARGIN_C).
MARGIN_C = 10


def lines(found):
    return _lines([(x[0], None) for x in found])

TEMPS = re.compile(r"^temperatures \(one metrics transfer.*?\): edge (\d+) C, hotspot (\d+) C, memory (\d+) C, counter (\d+)")
LIMITS = re.compile(r"^thermal limits \(the card's table\): edge (\d+) C, hotspot (\d+) C, memory (\d+) C .*software shutdown (\d+) C")


def reads(p):
    """Per read (after pmfw, at the end): (line no, when, temps or None, limits or None, the read's own text)."""
    out = []
    for when in ("pmfw", "ende"):
        mine = [(n, t) for n, w, t in p.thermo if w == when]
        if not mine:
            continue
        temps = next(((n, TEMPS.match(t)) for n, t in mine if TEMPS.match(t)), None)
        lim = next((LIMITS.match(t) for _, t in mine if LIMITS.match(t)), None)
        said = next(((n, t) for n, t in mine if t.startswith("temperatures")), (mine[0][0], "no temperatures line"))
        out.append((said[0], when, [int(x) for x in temps[1].groups()] if temps else None, [int(x) for x in lim.groups()] if lim else None, said[1]))
    return out


def alarms(p):
    """Every stage's THERMO alarme line with entries in it: (line no, stage, (rose, fell, power controller))."""
    out = []
    for n, who, t in p.thermo:
        m = re.match(r"alarme=\+(\d+):\+(\d+):\+(\d+)", t)
        if m and any(int(x) for x in m.groups()):
            out.append((n, who, tuple(int(x) for x in m.groups())))
    return out


def r_fans(p):
    r = row(p, "pmfw")
    if r:
        return r
    rd = reads(p)
    if not rd:
        return "refuted", "pmfw passed and the run read no temperature after it"
    hot = [(n, w) for n, w, t, lim, _ in rd if t is None or lim is None or any(t[i] >= lim[i] - MARGIN_C for i in range(3))]
    if hot:
        return "refuted", f"a read that failed or a temperature within {MARGIN_C} C of its limit ({', '.join(w for _, w in hot)}; {lines(hot)})"
    if alarms(p):
        return "refuted", f"the thermal taker took alerts during the run ({lines(alarms(p))})"
    t = rd[-1][2]
    return part(f"no thermal alert during the run; the PMFW keeps the card cool by itself: {rd[-1][1]} edge {t[0]} C, hotspot {t[1]} C, memory {t[2]} C ({lines(rd)})", "the fans themselves (Georg listens) and a card under load")


def r_metrics_read(p):
    r = row(p, "pmfw")
    if r:
        return r
    rd = reads(p)
    if len(rd) < 2:
        return "not measurable", f"{len(rd)} of the two reads (after pmfw, at the end) are in the protocol"
    bad = [(n, w, s) for n, w, t, lim, s in rd if t is None or lim is None]
    if bad:
        return "refuted", f"a read did not answer OK or the table did not parse: {bad[0][2]} (L{bad[0][0]})"
    over = [(n, w) for n, w, t, lim, _ in rd if t[0] >= lim[0] or t[1] >= lim[1] or t[2] >= lim[2]]
    if over:
        return "refuted", f"a temperature at or above its limit ({lines(over)})"
    odd = [(n, w) for n, w, t, _, _ in rd if not all(15 <= x <= 90 for x in t[:3])]
    if odd:
        return "refuted", f"a temperature outside 15..90 C, which no idle card on a desk shows ({lines(odd)})"
    if rd[0][2][3] == rd[1][2][3]:
        return "refuted", f"MetricsCounter did not change between the two reads ({lines(rd)}): the PMFW may not write the table"
    return "confirmed", f"TransferTableSmu2Dram (0x12, argument 5) answered OK twice, the table parsed, MetricsCounter moved; " + "; ".join(f"{w}: edge {t[0]} C, hotspot {t[1]} C, memory {t[2]} C" for _, w, t, _, _ in rd) + f" ({lines(rd)})"


def r_thm_events(p):
    a = alarms(p)
    if not a:
        return "not measurable", "no thermal entry arrived during the run (none was expected: the run does not heat the card)"
    return part(f"the taker took {sum(sum(x[2]) for x in a)} entries: " + ", ".join(f"{who} {x}" for _, who, x in a) + f" ({lines(a)})", "what src_data[0] carried, in the state look's lines")


def r_thm_ack(p):
    a = [x for x in alarms(p) if x[2][2] > 0]
    if not a:
        return "not measurable", "no interrupt of the power controller arrived, so nothing was acknowledged"
    return part(f"{sum(x[2][2] for x in a)} power controller interrupts taken ({lines(a)})", "whether INT_ACK clears itself (the state look after them)")


def r_thm_shutdown(p):
    off = [(n, t) for n, _, t in p.thermo if "switched off in order" in t]
    if off:
        return "confirmed", f"the taker decided a power-off for the temperature ({lines(off)})"
    return "not measurable", "Georg decides before the round; the run cannot provoke it (no heating) and the taker decided none"


CHECKS = {
    "fans": r_fans,
    "metrics-read": r_metrics_read,
    "thm-events": r_thm_events,
    "thm-ack": r_thm_ack,
    "thm-sensors": nm("needs a card at its software shutdown temperature; the run does not heat it"),
    "thm-stage-delay": nm("needs a thermal entry during a long stage; none is provoked"),
    "thm-shutdown": r_thm_shutdown,
}
