"""The stages of a measurement run's protocol as gx-messung-eval.py judges them: each stage's register accesses against
its section of Linux's trace under the section's declared rules, against the fake card's protocol, and its stack
figure against the head's stage budget (plan/spec/gpu-rdna4.md section 6, "The measurement run")."""

from gx_messung_proto import Rules, access, apply_rules, compare, expand, fixture, mes_unit
from gx_messung_ring import ring_unit

# The sections of several stages and the stage that ends each: a stage before it is "equal so far" (the rest are
# one stage each, which ends its section when it passes).
LAST = {"psp-ring": "ta", "mes": "mesq"}


def card_list(st):
    return expand([a for a in (access(t) for _, t in st.spur) if a])


def evaluate_stages(p, root, fake):
    """Per stage: (name, verdict words, ok). Fills p.eval[name] with what a row check needs."""
    p.eval = {}
    rows = []
    sections = {}
    for name in p.order:
        st = p.stages[name]
        v = st.vergleich or {}
        sec = v.get("section", "-")
        # The rungs stand in no section: their lines name the units they repeat, judged apart below.
        if name not in ("sdmatest", "ringtest"):
            sections.setdefault(sec, []).append(name)
    for name in p.order:
        st = p.stages[name]
        v = st.vergleich or {}
        sec, said = v.get("section", "-"), v.get("said", "none")
        mine, mine_ok, waits = "", True, []
        if sec in ("-",):
            mine = "no comparison (refused before any access)"
        elif name == "sdmatest":
            unit = mes_unit(root)
            c = card_list(st)
            if len(c) % len(unit) != 0:
                mine, mine_ok = f"{len(c)} accesses are no whole number of the {len(unit)}-line flush through MES (handler accesses? see the card's line)", False
            else:
                diffs = [compare(unit_with(unit, k), c[k * len(unit):(k + 1) * len(unit)], True)[0] for k in range(len(c) // len(unit))]
                bad = [d for d in diffs if d]
                mine, mine_ok = (f"equal: {len(diffs)} flushes through MES" if not bad else f"differs: {bad[0]}"), not bad
        elif name == "ringtest":
            # rdna4::ringseq: the end-of-pipe enables (only in the call that set them, which begins with the gfx
            # pipe's read), the IB page's bind flush, the remap's HDP flush and ring test, the unbind flush.
            c = card_list(st)
            eop = bool(c) and c[0]["k"] == "R" and c[0].get("name") == "CP_INT_CNTL_RING0"
            d, answers = compare(ring_unit(root, eop), c, st.passed)
            mine, mine_ok = ("equal: Linux's forms of the rungs, line for line" + (f"; {len(answers)} answers not Linux's" if answers else "") if not d else f"differs: {d}"), not d
        else:
            stages_of = sections[sec]
            before = []
            for n in stages_of[:stages_of.index(name)]:
                if p.passed(n):
                    before += card_list(p.stages[n])
            whole = st.passed and LAST.get(sec, name) == name
            rules = Rules(p.rules.get(sec, []))
            c, notes, problem = apply_rules(root, before + card_list(st), rules)
            fix = fixture(root, sec)
            if problem:
                mine, mine_ok = f"differs: {problem}", False
            else:
                d, answers = compare(fix, c, whole)
                if d and not st.passed:
                    mine = f"the stage stopped; up to there: {d}"
                elif d:
                    mine, mine_ok = f"differs: {d}", False
                else:
                    mine = ("equal" if whole else "equal so far") + (f"; {len(answers)} answers of the card are not Linux's, the first: {answers[0]}" if answers else "")
                p.eval.setdefault("answers", {})[name] = answers
                for f, x in zip(fix, c):
                    if f["k"] == "WAIT" and x["k"] == "WAIT" and x in card_list(st):
                        waits.append((f, x))
            if notes:
                mine += " (" + "; ".join(notes) + ")"
        p.eval.setdefault("waits", {})[name] = waits
        card_ok = said in ("equal", "so-far")
        agree = card_ok == mine_ok or not st.passed
        fakesay = fake_compare(st, fake.stages.get(name) if fake else None)
        budget = int(p.head.get("stapel_budget", "0") or 0)
        deep = st.stapel is not None and budget and st.stapel > budget
        ok = st.passed and card_ok and mine_ok and st.weiter is not None and not deep
        words = f"{name:9} {'PASS' if st.passed else 'FAIL'}  card: {said} (answers {v.get('answers', '0')}, protocol line {v.get('at', '-')})  evaluator: {mine}"
        if not agree:
            words += "  DISAGREE: the kernel's comparer and this script say different things; the kernel's is the authority, look at both"
        if st.stapel is None or not budget:
            stack = f"stack {st.stapel} bytes (no budget in the head)"
        else:
            stack = f"stack {st.stapel} of {budget} bytes: " + ("OVER THE STAGE BUDGET (fakemodel::STAGE_STACK is too small for the real card's path)" if deep else "within the stage budget")
        words += f"\n          fake card: {fakesay}; time {st.zeit} us; {stack}"
        if st.stopp:
            words += f"\n          STOPP (line {st.stopp[0]}): {st.stopp[1]}"
        rows.append((name, words, ok))
    return rows


def unit_with(unit, k):
    return [dict(u, init=u["init"]) for u in unit]


def fake_compare(st, fk):
    if fk is None:
        return "no fake protocol for this stage"
    a, b = card_list(st), card_list(fk)
    if [(x["k"], x.get("dword")) for x in a] != [(x["k"], x.get("dword")) for x in b]:
        return f"the access list is not the fake's ({len(a)} accesses, the fake {len(b)})"
    diffs = []
    for x, y in zip(a, b):
        gx = x.get("last") if x["k"] == "WAIT" else x.get("value")
        gy = y.get("last") if y["k"] == "WAIT" else y.get("value")
        if x["k"] in ("R", "WAIT", "HDP") and gx != gy:
            diffs.append(f"{x.get('name')}: card {('-' if gx is None else f'{gx:#010x}')}, fake {('-' if gy is None else f'{gy:#010x}')}")
    return "the same answers as the fake" if not diffs else f"{len(diffs)} answers not the fake's, the first {diffs[0]}"


