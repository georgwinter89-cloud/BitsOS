#!/usr/bin/env python3
"""Reads the protocol of the BitsOS measurement run of the RX 9070 XT (`gpu-messung`, \\BITSOS2\\REPORT\\GPUMnnnn.TXT)
and says, stage by stage, whether the real card did what Linux's trace and the fake card say, then walks every row
of plan/spec/gpu-rdna4.md section 7 by its key and says for each: confirmed, refuted (each with the protocol's
lines), or not measurable in this run (why).

  python3 host/gaming-pc/gx-messung-eval.py <protocol> [--fake <protocol of the fake card>] [--root <repo>]

Per stage it compares the recorded register accesses (SPUR lines, the fixtures' line forms) with the stage's
section of Linux's trace (crates/drivers/amdgpu/src/rdna4/linuxseq/<section>.txt) under the section's declared
rules as the program wrote them (REGELN lines): the sequence's skeleton (kind, register, written value except an
address a rule names, a wait's condition and bound), the declared extra lines (a bind's TLB flush, an HDP flush,
the untraced read, the rounds of a pair poll), and separately the card's answers (plain reads, a wait's last value,
the memory size read of an HDP flush). The kernel's own comparer (VERGLEICH, the stage's linuxseq line) is the
authority: this script says where it agrees and names a disagreement. With the fake card's protocol
(host/tests/gx/fixtures/gpu-messung-fake.txt by default) it names each answer the real card gave otherwise.

A row is "confirmed in part" when the protocol holds only part of what the row's last column checks; the line names
the part that is not measured in this run. Each stage's stack figure is judged against the head's stage budget.

Exit 0: every stage that ran agrees and no row is refuted; 1: a difference, a stop, a stage over the stack budget,
a refuted row or an error of this script; 2: the file is no protocol of this format.
"""

import argparse
import os
import sys

# Its modules lie beside it; no bytecode is left in the tree.
sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from gx_messung_proto import Proto  # noqa: E402
from gx_messung_stages import evaluate_stages  # noqa: E402
from gx_messung_rows import CHECKS, spec_keys  # noqa: E402

FORMAT = "1"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("protocol")
    ap.add_argument("--fake")
    ap.add_argument("--root")
    a = ap.parse_args()
    root = a.root or os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "../.."))
    p = Proto(a.protocol)
    if p.head.get("format") != FORMAT:
        print(f"{a.protocol}: no protocol of format {FORMAT} (KOPF format={p.head.get('format')})")
        return 2
    fake_path = a.fake or os.path.join(root, "host/tests/gx/fixtures/gpu-messung-fake.txt")
    fake = Proto(fake_path) if os.path.isfile(fake_path) else None
    bad = False
    print(f"gx-messung-eval: {a.protocol}")
    print(f"kernel {p.head.get('kernel')}, card {p.head.get('karte')}, log level {p.head.get('loglevel')}")
    for n, name, word, text in p.checks:
        if word != "ok":
            print(f"preflight {name}: {word} {text} (L{n})")
            bad = True
    if p.end is None:
        print(f"NO END LINE: the run hung or the power went off {p.hung()}")
        bad = True
    else:
        print(f"end (L{p.end[0]}): {p.end[1]}")
    print("== stages")
    for name, words, ok in evaluate_stages(p, root, fake):
        print(words)
        bad |= not ok
    if p.stops:
        n, who, t = p.stops[0]
        print(f"stopped at {who} (L{n}): {t}")
    print("== section 7 (plan/spec/gpu-rdna4.md)")
    keys = spec_keys(root)
    count = {"confirmed": 0, "in part": 0, "refuted": 0, "not measurable": 0}
    for key, title in keys:
        f = CHECKS.get(key)
        if f is None:
            print(f"{key:16} NO CHECK in this script for this row: {title}")
            bad = True
            continue
        try:
            verdict, why = f(p)
        except Exception as e:  # noqa: BLE001 - a broken check is this script's error, never a verdict
            print(f"{key:16} EVALUATOR ERROR {type(e).__name__}: {e}")
            bad = True
            continue
        count[verdict] = count.get(verdict, 0) + 1
        bad |= verdict == "refuted"
        word = "confirmed in part" if verdict == "in part" else verdict
        print(f"{key:16} {word:17} {why}")
    for key in CHECKS:
        if key not in [k for k, _ in keys]:
            print(f"{key:16} a check of this script that is no row of section 7")
            bad = True
    print(f"== {count['confirmed']} confirmed, {count['in part']} confirmed in part, {count['refuted']} refuted, {count['not measurable']} not measurable")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
