#!/usr/bin/env python3
"""SX-3b: compares the TAP lines of bubblewrap's suites on V2 with the CachyOS transcripts.

Arguments: <expected dir> <v2 dir> [suite ...]. expected/<suite>.tap holds CachyOS's lines (comments "# ..." and
blank lines ignored); expected/known.txt lists differences V2 may show, one per line:
    <suite> | <CachyOS line> | <V2 line> | <owner>: <reason>
Each CachyOS line must come back the same at its place, or as its known V2 line. A known line that came back the
same is reported ("can go"). The V2 line "(stops)" means the suite ends there on V2 (a known abort): the
lines after it are not compared and are counted, loudly (the summary line says TRUNCATED). A suite that stopped early (set -e: the first failing check ends test-run.sh) is red,
with the last lines of its trace."""
import os
import sys

EXP, V2 = sys.argv[1], sys.argv[2]
SUITES = sys.argv[3:] or ["test-run", "test-specifying-pidns", "test-specifying-userns", "test-utils"]


def tap(path):
    with open(path) as f:
        return [l.rstrip() for l in f if l.startswith(("ok ", "not ok ", "1.."))]


known = {}
with open(os.path.join(EXP, "known.txt")) as f:
    for l in f:
        if l.strip() and not l.startswith("#"):
            suite, host, v2, why = [x.strip() for x in l.split("|", 3)]
            known[(suite, host)] = (v2, why)

red, used, gone, truncated = 0, 0, [], []
for s in SUITES:
    want = tap(os.path.join(EXP, f"{s}.tap"))
    try:
        got = tap(os.path.join(V2, f"{s}.tap"))
    except OSError:
        got = []
    bad = []
    for i, w in enumerate(want):
        g = got[i] if i < len(got) else "(missing)"
        k = known.get((s, w))
        if g == w:
            if k:
                gone.append(f"{s} | {w}")
            continue
        if k and g == k[0]:
            used += 1
            print(f"known {s}: {g}  ({k[1]})")
            continue
        if k and k[0] == "(stops)" and g == "(missing)":
            used += 1
            print(f"known {s}: stops at '{w}' ({k[1]}); {len(want) - i - 1} later lines not checked")
            truncated.append(f"{s} after '{w}' ({len(want) - i - 1} lines)")
            got = got[:i]
            break
        bad.append(f"  CachyOS: {w}\n  V2:      {g}")
    extra = got[len(want):]
    if extra:
        bad.append("  V2 has more lines: " + "; ".join(extra[:5]))
    if bad:
        red += 1
        print(f"RED {s}: {len(bad)} line(s) differ")
        print("\n".join(bad[:20]))
        try:
            with open(os.path.join(V2, f"{s}.err")) as f:
                tail = f.read().splitlines()[-25:]
            print("  --- last trace lines on V2 ---\n  " + "\n  ".join(tail))
        except OSError:
            pass
    else:
        print(f"ok {s}: {len(want)} lines as on CachyOS")
for g in gone:
    print(f"note: known line passes now, it can go from known.txt: {g}")
cut = f"; TRUNCATED: {', '.join(truncated)} not checked" if truncated else ""
print(f"bwrap suites: {'RED' if red else 'green'} ({len(SUITES) - red}/{len(SUITES)} suites, {used} known lines{cut})")
sys.exit(1 if red else 0)
