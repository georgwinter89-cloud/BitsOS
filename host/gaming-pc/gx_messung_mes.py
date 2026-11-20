"""The section-7 checks of MES (wave 44: the stages mes and mesq, the TLB flush through MES), for
gx-messung-eval.py: each says confirmed, refuted, in part or not measurable with the protocol's lines."""

import re

from gx_messung_help import lines, nm, part, row


def commands(p, stage):
    """A stage's MES command lines: (answered, not answered)."""
    if not p.ran(stage):
        return [], []
    ok = p.stages[stage].has(r"^MES\(\d\) \S+.*: answered after \d+ polls")
    bad = p.stages[stage].has(r"MES\(\d\) (failed to respond|msg=.*ring buffer is full)")
    return ok, bad


def r_answers(p):
    r = row(p, "mes")
    if r:
        return r
    ok, bad = commands(p, "mes")
    ok2, bad2 = commands(p, "mesq")
    unchecked = [b for b in bad + bad2 if "does not check this answer" in b[1]]
    if [b for b in bad + bad2 if b not in unchecked]:
        return "refuted", f"a command that stops the stage got no good answer ({lines(bad + bad2)})"
    polls = [int(m.group(1)) for _, t in ok + ok2 for m in [re.search(r"answered after (\d+) polls", t)] if m]
    most = max(polls) if polls else 0
    if unchecked:
        return part(f"{len(ok + ok2)} commands answered, the slowest after {most} polls of 2 us ({lines(ok + ok2)})", f"{len(unchecked)} commands whose answer Linux does not check got none ({lines(unchecked)})")
    return "confirmed", f"{len(ok + ok2)} commands answered with status 1, the slowest after {most} polls of 2 us ({lines(ok + ok2)})"


def r_version(p):
    r = row(p, "mes")
    if r:
        return r
    v = p.stages["mes"].has(r"^mes: kiq version 0x[0-9a-f]+, scheduler version")
    if not v:
        return "refuted", "mes passed without its version line"
    good = "kiq version 0x1029097, scheduler version 0x1029097" in v[0][1]
    return ("confirmed" if good else "refuted"), f"{v[0][1][:200]} ({lines(v)})"


def r_start(p):
    r = row(p, "mes")
    if r:
        return r
    ans = p.stages["mes"].vergleich.get("answers", "0")
    first = p.stages["mes"].has(r"^MES\(1\) SET_HW_RSRC: answered")
    if ans != "0":
        return "refuted", f"{ans} answers of the card in the section mes are not the trace's (the cold CP_MES_CNTL, the HQD reads, CP_MES_GP3_LO): see the stage's linuxseq line"
    if not first:
        return "refuted", "the KIQ pipe's first command got no answer after the 500 us"
    return "confirmed", f"the cold reads are the trace's and the KIQ pipe answered its first command after the 500 us ({lines(first)})"


def r_kiq_1(p):
    r = row(p, "mes")
    if r:
        return r
    a = p.stages["mes"].has(r"^MES\([01]\) SET_HW_RSRC_1: answered")
    n = p.stages["mes"].has(r"msg=SET_HW_RSRC_1")
    if len(a) == 2:
        return "confirmed", f"both pipes answered SET_HW_RESOURCES_1 ({lines(a)}): the tolerance Linux has was not needed"
    return part(f"{len(a)} of 2 answered; the stage went on as Linux does ({lines(n)})", "why the firmware did not answer")


def r_flags(p):
    r = row(p, "mes")
    if r:
        return r
    a = p.stages["mes"].has(r"^MES\([01]\) SET_HW_RSRC: answered")
    return ("confirmed" if len(a) == 2 else "refuted"), f"SET_HW_RESOURCES with the flags 0x100447 and the timer 50 answered on {len(a)} of 2 pipes ({lines(a)})"


def r_doorbell(p):
    r = row(p, "mes")
    if r:
        return r
    kiq = p.stages["mes"].has(r"^MES\(1\) .*: answered")
    sched = p.stages["mes"].has(r"^MES\(0\) .*: answered")
    if kiq and sched:
        return "confirmed", f"commands on both rings answered: BAR2 dwords 0x18 and 0x16 start them ({lines(kiq[:1] + sched[:1])}); the aggregated doorbells are only programmed"
    return "refuted", f"KIQ ring {len(kiq)}, scheduler ring {len(sched)} answered commands"


def r_wr_wait(p):
    r = row(p, "sdma")
    if r:
        return r
    bad = [x for s in ("sdma", "sdmatest") if p.ran(s) for x in p.stages[s].has(r"^tlb flush: MES")]
    ok = p.stages["sdma"].has("ring test passed")
    if bad:
        return "refuted", f"flushes through MES not answered ({lines(bad)})"
    if len(ok) == 2:
        return "confirmed", f"the engines' bind was flushed through MES and both found their rings ({lines(ok)})"
    return "refuted", f"ring tests: {len(ok)} of 2"


def r_flush_mes(p):
    r = row(p, "sdma")
    if r:
        return r
    v = p.stages["sdma"].vergleich
    t = p.stages["sdmatest"].vergleich if p.ran("sdmatest") else None
    good = v.get("said") == "equal" and (t is None or t.get("said") == "equal")
    return ("confirmed" if good else "refuted"), f"sdma {v.get('said')} under BindMesBefore (three HDP flushes), sdmatest {t.get('said') if t else 'not run'} (L{v.get('at')})"


def r_flush_timeout(p):
    bad = [x for s in ("sdma", "sdmatest") if p.ran(s) for x in p.stages[s].has(r"^tlb flush: MES")]
    if not bad:
        return "not measurable", "every flush through MES was answered: the timeout's path did not run"
    return part(f"{len(bad)} flushes were not answered and the stages went on, as Linux does ({lines(bad)})", "what MES did with them")


def r_queues(p):
    r = row(p, "mesq")
    if r:
        return r
    adds = p.stages["mesq"].has(r"^MES\(0\) ADD_QUEUE: answered")
    q = p.stages["mesq"].has(r"^mesq: (compute|gfx) queue me")
    return ("confirmed" if len(adds) == 5 and len(q) == 5 else "refuted"), f"{len(adds)} of 5 kernel queues mapped through MES ({lines(adds)}); their rings are tested by the rings' stage"


def r_isolation(p):
    r = row(p, "mes")
    if r:
        return r
    c = p.stages["mes"].has(r"MISC \(CHANGE_CONFIG\): answered")
    on = p.stages["mes"].has(r"cleaner shader on: CHANGE_CONFIG sent")
    if on and c:
        return "confirmed", f"CHANGE_CONFIG (limit_single_process 0) answered ({lines(c)})"
    if not on:
        return "refuted", "the files' versions turned the cleaner shader off: no CHANGE_CONFIG went out, unlike Linux on this card"
    return "refuted", "CHANGE_CONFIG went out without an answer"


def r_cleaner(p):
    r = row(p, "mes")
    if r:
        return r
    v = p.stages["mes"].has(r"cleaner shader on: CHANGE_CONFIG sent \(ME 0x([0-9a-f]+), PFP 0x([0-9a-f]+), MEC 0x([0-9a-f]+), MES 0x([0-9a-f]+)\)")
    if not v:
        return "refuted", "the cleaner shader is off by the files' versions"
    m = re.search(r"ME 0x([0-9a-f]+), PFP 0x([0-9a-f]+), MEC 0x([0-9a-f]+), MES 0x([0-9a-f]+)", v[0][1])
    got = tuple(int(x, 16) for x in m.groups())
    linux = (0xC26, 0xC94, 0xDA2, 0x97)
    return ("confirmed" if got == linux else "in part"), f"ME {got[0]:#x}, PFP {got[1]:#x}, MEC {got[2]:#x}, MES {got[3]:#x} (Linux's of 18.11.: 0xc26, 0xc94, 0xda2, 0x97) ({lines(v)})"


CHECKS = {
    "mes-answers": r_answers,
    "mes-version": r_version,
    "mes-start": r_start,
    "mes-kiq-1": r_kiq_1,
    "mes-flags": r_flags,
    "mes-resource1": nm("nothing reads the resource_1 pages: what MES writes there is not looked at"),
    "mes-ring-memory": r_answers,
    "mes-doorbell": r_doorbell,
    "mes-wr-wait": r_wr_wait,
    "flush-mes": r_flush_mes,
    "mes-flush-timeout": r_flush_timeout,
    "mes-queues": r_queues,
    "mes-reload": nm("needs a Reload after mes; the run switches the PC off instead"),
    "mes-isolation": r_isolation,
    "mes-cleaner-versions": r_cleaner,
    "mes-file-version": nm("the file's own version dword lies in its body, which BitsOS does not read"),
    "mes-fault-client": nm("needs a fault of MES; none is provoked"),
}
