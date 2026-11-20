"""Helpers of the section-7 checks (gx_messung_rows.py, gx_messung_late.py): a row that stands on stages, the
protocol's line numbers as evidence, the IH counts over the run."""

import re


def lines(found):
    return ", ".join(f"L{n}" for n, _ in found[:6]) or "-"


def need(p, *stages):
    for s in stages:
        if not p.ran(s):
            return f"the stage {s} did not run in this run"
        if not p.passed(s):
            return f"the stage {s} did not pass"
    return None


def row(p, *stages):
    """A row that stands on stages: (not measurable, why) when one did not pass."""
    why = need(p, *stages)
    return ("not measurable", why) if why else None


def check_lines(p, stages, pat, want_all=True):
    found = []
    for s in stages:
        found += p.stages[s].has(pat)
    return found


def ih_total(p, field):
    total = 0
    for _, t in p.ihs.values():
        m = re.search(field + r"=\+(\d+)", t)
        if m:
            total += int(m.group(1))
    return total


def nm(why):
    return lambda p: ("not measurable", why)


def r_simple(stage, pat, also=()):
    def f(p):
        r = row(p, stage, *also)
        if r:
            return r
        found = p.stages[stage].has(pat)
        if not found:
            return "refuted", f"the stage {stage} passed without the line /{pat}/"
        return "confirmed", f"{found[0][1][:160]} ({lines(found)})"
    return f


def part(measured, missing):
    """A row the protocol holds only in part: what it shows, and what the row's check needs that this run does not
    read."""
    return "in part", f"{measured}; not measured in this run: {missing}"


def ih_after(p, stage):
    """The IH lines of `stage` and every later one (each counts since the record before), and of the end."""
    names = p.order[p.order.index(stage):] if stage in p.order else []
    return [p.ihs[n] for n in names + ["ende"] if n in p.ihs]


def ih_clients(t):
    """The IH clients an IH line lists as unclaimed or taken (`client:source:+n`, `who:client:source:ring:+n`)."""
    out = set()
    m = re.search(r" unclaimed=(.*?) taken=(.*?) refused=", t)
    if not m:
        return out
    for x in m.group(1).split(","):
        if x != "-" and x.count(":") >= 2:
            out.add(int(x.split(":")[0]))
    for x in m.group(2).split(","):
        if x != "-" and x.count(":") >= 4:
            out.add(int(x.rsplit(":", 4)[1]))
    return out
