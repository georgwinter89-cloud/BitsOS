"""The measurement run's protocol as gx-messung-eval.py reads it: the lines by their keys, the register accesses
in the fixtures' forms, and the comparison of a stage's accesses with its section of Linux's trace under the
section's declared rules (crates/drivers/amdgpu/src/rdna4/linuxseq, plan/spec/gpu-rdna4.md section 6)."""

import os
import re

class Stage:
    def __init__(self, name, nr, line):
        self.name, self.nr, self.at = name, nr, line
        self.answer = []  # (line no, text)
        self.zeit = self.stapel = None
        self.vergleich = None  # dict
        self.spur = []  # (line no, text)
        self.spurinfo = ""
        self.ih = None
        self.weiter = self.stopp = None  # (line no, text)

    def has(self, pat):
        rx = re.compile(pat)
        return [(n, t) for n, t in self.answer if rx.search(t)]

    @property
    def passed(self):
        return any(re.search(r"gx_bringup \S+ \S+: PASS$", t) for _, t in self.answer)


class Proto:
    def __init__(self, path):
        self.path = path
        self.head, self.rules, self.checks, self.files, self.gpu = {}, {}, [], [], {"vorher": [], "nachher": []}
        self.stages, self.order, self.looks, self.logs, self.ihs = {}, [], [], [], {}
        # Wave 44: the thermal taker's lines, (line no, after which stage or "ende", text).
        self.thermo = []
        self.end = None
        self.stops = []
        # What was started and not closed when the file ends: ("stage", name, line), ("look", look, after, line).
        self.open = None
        self.lines = []
        with open(path, encoding="utf-8", errors="replace") as f:
            self.lines = f.read().split("\n")
        cur = None
        for i, raw in enumerate(self.lines, 1):
            if not raw:
                continue
            key, _, rest = raw.partition(" ")
            if key == "KOPF":
                k, _, v = rest.partition("=")
                self.head[k] = v
            elif key == "PRUEF":
                p = rest.split(" ", 2)
                self.checks.append((i, p[0], p[1] if len(p) > 1 else "", p[2] if len(p) > 2 else ""))
            elif key == "FIRMWARE":
                self.files.append((i, rest))
            elif key == "GPU":
                w, _, t = rest.partition(" ")
                self.gpu.setdefault(w, []).append((i, t))
            elif key == "START":
                p = rest.split(" ")
                nr = int(dict(x.split("=", 1) for x in p[1:] if "=" in x).get("nr", "0"))
                cur = Stage(p[0], nr, i)
                self.stages[p[0]] = cur
                self.order.append(p[0])
                self.open = ("stage", p[0], i)
            elif key in ("ANTWORT", "ZEIT", "STAPEL", "VERGLEICH", "SPUR", "SPURINFO", "IH", "WEITER", "STOPP", "REGELN"):
                who, _, t = rest.partition(" ")
                st = self.stages.get(who)
                if key == "REGELN":
                    self.rules.setdefault(who, []).append((i, t))
                elif key == "IH":
                    self.ihs[who] = (i, t)
                    if st:
                        st.ih = (i, t)
                elif key == "STOPP":
                    self.stops.append((i, who, t))
                    self.open = None
                    if st:
                        st.stopp = (i, t)
                elif st is None:
                    continue
                elif key == "ANTWORT":
                    st.answer.append((i, t))
                elif key == "ZEIT":
                    st.zeit = int(t.split("=")[1])
                elif key == "STAPEL":
                    st.stapel = int(t.split("=")[1])
                elif key == "VERGLEICH":
                    st.vergleich = dict(x.split("=", 1) for x in t.split(" ") if "=" in x)
                    st.vergleich["at"] = i
                elif key == "SPUR":
                    st.spur.append((i, t))
                elif key == "SPURINFO":
                    st.spurinfo = t
                elif key == "WEITER":
                    st.weiter = (i, t)
                    self.open = None
            elif key == "BLICK":
                look, _, t = rest.partition(" ")
                after, _, t = t.partition(" ")
                after = after.replace("nach=", "")
                if t == "START":
                    self.open = ("look", look, after, i)
                    continue
                if re.match(r"writes=\d+ |refused ", t):
                    self.open = None
                self.looks.append((i, look, after, t))
            elif key == "THERMO":
                who, _, t = rest.partition(" ")
                self.thermo.append((i, who, t))
            elif key == "LOG":
                self.logs.append((i, rest))
            elif key == "ENDE":
                self.end = (i, rest)

    def ran(self, name):
        return name in self.stages

    def hung(self):
        """Where a file without its end line stopped, in words."""
        o = self.open
        if o and o[0] == "stage":
            return f"in the stage {o[1]} (its START at L{o[2]} is the last thing begun; none of its accesses is written, they are written when a stage returns)"
        if o and o[0] == "look":
            return f"in the look {o[1]} after {o[2]} (its START at L{o[3]}); the stage before it is whole in the file"
        if not self.order:
            return "in the preflight, before any stage started"
        return f"after {self.order[-1]}, between two writes (the last part begun is whole)"

    def passed(self, name):
        s = self.stages.get(name)
        return bool(s and s.passed and s.weiter)

    def look(self, look, after, pat):
        rx = re.compile(pat)
        return [(n, t) for n, l, a, t in self.looks if l == look and a == after and rx.search(t)]


# ---- lines in the fixtures' forms

def hexi(s):
    return int(s, 16) if s not in ("-", "") else None


def access(text):
    """One SPUR line or one fixture line (without its init.txt number) as a dict, or None."""
    t = text.split(" ")
    k = t[0]
    if k == "W" and len(t) >= 4:
        return {"k": "W", "name": t[1], "dword": hexi(t[2]), "value": hexi(t[3]), "reached": len(t) < 5}
    if k == "R" and len(t) >= 4:
        times = int(t[4][1:]) if len(t) > 4 and t[4].startswith("x") else 1
        return {"k": "R", "name": t[1], "dword": hexi(t[2]), "value": hexi(t[3]), "times": times}
    if k == "WAIT" and len(t) >= 4:
        f = dict(x.split("=", 1) for x in t[3:] if "=" in x)
        cond = ("changed", f.get("from")) if t[3] == "changed" else ("mask", f.get("mask"), f.get("is"))
        return {"k": "WAIT", "name": t[1], "dword": hexi(t[2]), "cond": cond, "max": f.get("max"), "last": hexi(f.get("last", "-")), "reads": int(f.get("reads", "0")), "us": int(f.get("us", "0"))}
    if k == "HDP" and len(t) >= 5:
        return {"k": "HDP", "flush": hexi(t[1]), "name": t[2], "mem": hexi(t[3]), "value": hexi(t[4])}
    if k == "SLEEP":
        f = dict(x.split("=", 1) for x in t[1:] if "=" in x)
        return {"k": "SLEEP", "us": int(f.get("us", "0"))}
    return None


def expand(items):
    out = []
    for a in items:
        if a["k"] == "R" and a.get("times", 1) > 1:
            for _ in range(a["times"]):
                b = dict(a)
                b["times"] = 1
                out.append(b)
        else:
            out.append(a)
    return out


def fixture(root, section):
    path = os.path.join(root, "crates/drivers/amdgpu/src/rdna4/linuxseq", section + ".txt")
    out = []
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.rstrip("\n")
            if not line or line.startswith("#"):
                continue
            init, _, rest = line.partition(" ")
            a = access(rest)
            if a is None:
                raise SystemExit(f"{path}: no fixture line: {line}")
            a["init"] = int(init)
            out.append(a)
    return expand(out)


def bind_unit(root, both):
    """Linux's TLB flush after a bind (linuxseq::bind_flush), and with GFXHUB on (bind_flush_both)."""
    s = fixture(root, "sw-init")
    end = None
    for i in range(len(s) - 1):
        if s[i]["k"] == "W" and s[i]["name"] == "MMVM_L2_BANK_SELECT_RESERVED_CID2" and s[i + 1]["k"] == "R" and s[i + 1]["name"] == "MMVM_L2_BANK_SELECT_RESERVED_CID2":
            end = i + 2
            break
    mm = s[:end]
    if not both:
        return mm
    gc = fixture(root, "gc-hub")[-3:]
    return mm[:1] + gc + mm[1:]


def mes_unit(root):
    """Linux's TLB flush after a bind once MES runs (linuxseq::bind_flush_mes): three HDP flushes, the hubs'
    invalidations MES commands (gmc_v12_0.c:305-330)."""
    hdp = next((a for a in fixture(root, "mes") if a["k"] == "HDP"), None)
    if hdp is None:
        raise SystemExit("mes.txt holds no HDP flush")
    return [dict(hdp) for _ in range(3)]


def kind_of(a):
    return {"HDP": "H", "WAIT": "T", "W": "W", "R": "R", "SLEEP": "S"}[a["k"]]


def say(a):
    if a["k"] == "W":
        return f"writes {a['value']:#010x} to {a['name']}"
    if a["k"] == "R":
        return f"reads {a['name']}"
    if a["k"] == "WAIT":
        return f"waits on {a['name']} ({' '.join(str(x) for x in a['cond'])}, max {a['max']})"
    if a["k"] == "HDP":
        return f"flushes the HDP through dword {a['flush']:#x}"
    return f"sleeps {a['us']} us"


class Rules:
    def __init__(self, lines):
        self.address, self.hdp, self.bind, self.bind_both, self.bind_mes, self.untraced, self.args, self.pairs = set(), set(), set(), set(), set(), [], [], []
        for _, t in lines:
            p = t.split(" ")
            kind, names = p[0], (p[1].split(",") if len(p) > 1 else [])
            f = dict(x.split("=", 1) for x in p[2:] if "=" in x)
            if kind == "address":
                self.address.update(names)
            elif kind == "hdp-before":
                self.hdp.update(names)
            elif kind == "bind-before":
                self.bind.update(names)
            elif kind == "bind-both-before":
                self.bind_both.update(names)
            elif kind == "bind-mes-before":
                self.bind_mes.update(names)
            elif kind == "untraced":
                self.untraced.append(int(f["byte"], 16) // 4)
            elif kind == "argument":
                self.args.append((names[0], names[1], int(f["hi"], 16), int(f["lo"], 16)))
            elif kind == "pair-poll":
                self.pairs.append((names[0], names[1]))


def apply_rules(root, card, rules):
    """The card's list with the lines a declared rule allows taken out, and the values a rule decides marked.
    Returns (list, notes, problem)."""
    notes = []
    c = list(card)
    for d in rules.untraced:
        if c and c[0]["k"] == "R" and c[0]["dword"] == d:
            notes.append("the untraced read before the section is there")
            c = c[1:]
    for unit, regs in ((bind_unit(root, False), rules.bind), (bind_unit(root, True), rules.bind_both), (mes_unit(root), rules.bind_mes)):
        for reg in regs:
            at = next((i for i, a in enumerate(c) if a["k"] == "W" and a["name"] == reg), None)
            if at is None or at < len(unit):
                return c, notes, f"the bind's TLB flush the rule declares before {reg} is not there"
            block = c[at - len(unit):at]
            for x, y in zip(block, unit):
                if kind_of(x) != kind_of(y) or (x.get("dword"), x.get("flush")) != (y.get("dword"), y.get("flush")) or (x["k"] == "W" and x["value"] != y["value"]):
                    return c, notes, f"the TLB flush before {reg} is not Linux's: the card list {say(x)} where Linux {say(y)}"
            notes.append(f"the bind's TLB flush before {reg} is Linux's ({len(unit)} lines)")
            c = c[:at - len(unit)] + c[at:]
    if rules.hdp:
        out = []
        for i, a in enumerate(c):
            nxt = c[i + 1] if i + 1 < len(c) else None
            if a["k"] == "HDP" and nxt and nxt["k"] == "W" and nxt["name"] in rules.hdp:
                continue
            out.append(a)
        c = out
    for first, second in rules.pairs:
        out, i, rounds = [], 0, 0
        while i < len(c):
            pair = i + 3 < len(c) and all(c[i + j]["k"] == "R" for j in range(4)) and (c[i]["name"], c[i + 1]["name"], c[i + 2]["name"], c[i + 3]["name"]) == (first, second, first, second)
            if pair:
                i += 2
                rounds += 1
                continue
            out.append(c[i])
            i += 1
        if rounds:
            notes.append(f"the pair poll took {rounds + 1} rounds (Linux: 1); the rounds before the last are left out")
        c = out
    for i, a in enumerate(c):
        if a["k"] == "W" and a["name"] in rules.address:
            a["free"] = True
        for arg, msg, hi, lo in rules.args:
            nxt = c[i + 1] if i + 1 < len(c) else None
            if a["k"] == "W" and a["name"] == arg and nxt and nxt["k"] == "W" and nxt["name"] == msg and nxt["value"] in (hi, lo):
                a["free"] = True
    return c, notes, None


def compare(fix, card, whole):
    """(first difference or None, the card's answers that are not Linux's)."""
    answers = []
    for i, (f, c) in enumerate(zip(fix, card)):
        where = f"init.txt line {f['init']}"
        if kind_of(f) != kind_of(c) or (f.get("dword") is not None and f.get("dword") != c.get("dword")):
            return f"{where}: Linux {say(f)}; the card's stage {say(c)}", answers
        if f["k"] == "W" and not c.get("free") and f["value"] != c["value"]:
            return f"{where}: Linux {say(f)}; the stage writes {c['value']:#010x}", answers
        if f["k"] == "W" and not c.get("reached", True):
            return f"{where}: the write did not reach the card", answers
        if f["k"] == "WAIT" and (f["cond"] != c["cond"] or f["max"] != c["max"]):
            return f"{where}: Linux {say(f)}; the stage {say(c)}", answers
        if f["k"] == "HDP" and (f["flush"], f["mem"]) != (c["flush"], c["mem"]):
            return f"{where}: Linux {say(f)}; the stage {say(c)}", answers
        if f["k"] == "SLEEP" and f["us"] != c["us"]:
            return f"{where}: Linux sleeps {f['us']} us; the stage {c['us']} us", answers
        got = c.get("last") if c["k"] == "WAIT" else c.get("value")
        want = f.get("last") if f["k"] == "WAIT" else f.get("value")
        if c["k"] in ("R", "WAIT", "HDP") and got != want:
            answers.append(f"{where}: {f['name']} {want:#010x} for Linux, {('-' if got is None else f'{got:#010x}')} on the card")
    if len(card) > len(fix):
        return f"the stage makes {len(card) - len(fix)} accesses more than Linux's section has: first {say(card[len(fix)])}", answers
    if whole and len(card) < len(fix):
        return f"Linux's section has {len(fix) - len(card)} lines more: first {say(fix[len(card)])} (init.txt line {fix[len(card)]['init']})", answers
    return None, answers
