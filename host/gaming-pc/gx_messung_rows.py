"""The rows of plan/spec/gpu-rdna4.md section 7, one check per key, for gx-messung-eval.py: each says confirmed or
refuted with the protocol's lines, or not measurable in this run and why."""

import os
import re

from gx_messung_help import check_lines, ih_after, ih_total, lines, nm, part, r_simple, row
from gx_messung_cp import CP_CHECKS
from gx_messung_mes import CHECKS as MES_CHECKS
from gx_messung_ring import CHECKS as RING_CHECKS
from gx_messung_therm import CHECKS as THERMAL
from gx_messung_late import (r_after_failure, r_bootload_value, r_enable_time, r_fill_form, r_flush_both, r_gchub, r_gcboot, r_grbm, r_irq, r_layout, r_link, r_pcie, r_pmfw_answers, r_pptlimit, r_pulses, r_rundcbtc, r_rungs, r_scpm, r_video)

def spec_keys(root):
    path = os.path.join(root, "plan/spec/gpu-rdna4.md")
    keys, inside = [], False
    with open(path, encoding="utf-8") as f:
        for line in f:
            if line.startswith("## 7."):
                inside = True
                continue
            if inside and line.startswith("## "):
                break
            m = re.match(r"\| `([a-z0-9-]+)` \| (.*?) \|", line)
            if inside and m:
                keys.append((m.group(1), m.group(2)))
    return keys


def r_accesses(p):
    chain = [s for s in p.order if p.stages[s].vergleich and p.stages[s].vergleich.get("section") not in (None, "-")]
    if not chain:
        return "not measurable", "no stage ran"
    bad = [(p.stages[s].vergleich["at"], s) for s in chain if p.stages[s].vergleich.get("said") not in ("equal", "so-far")]
    ev = [(p.stages[s].vergleich["at"], s) for s in chain]
    if bad:
        return "refuted", f"{', '.join(s for _, s in bad)} not Linux's ({lines(bad)})"
    return "confirmed", f"{len(chain)} stages equal or equal so far ({lines(ev)})"


def r_wait_time(p):
    all_w = [(s, f, x) for s, ws in p.eval.get("waits", {}).items() for f, x in ws]
    if not all_w:
        return "not measurable", "no wait was compared"
    out = [x for _, f, x in all_w if ran_out(x)]
    slow = sorted(all_w, key=lambda w: -w[2]["us"])[:3]
    desc = "; ".join(f"{s} {x['name']} {x['reads']} reads in {x['us']} us (Linux {f['reads']} in {f['us']} us)" for s, f, x in slow)
    if out:
        return "refuted", f"{len(out)} waits ran to Linux's bound; the longest: {desc}"
    return "confirmed", f"{len(all_w)} waits within Linux's bounds; the longest: {desc}"


def ran_out(x):
    """A wait that used its whole bound: Linux's read count, or its time (`max=time:<us>us/<step>us`)."""
    if x["max"].isdigit():
        return x["reads"] >= int(x["max"])
    m = re.match(r"time:(\d+)us/\d+us$", x["max"] or "")
    if m:
        return x["us"] >= int(m.group(1))
    raise ValueError(f"a wait bound this script cannot read: max={x['max']}")


def r_answers(p):
    ran = [s for s in p.order if p.stages[s].vergleich and p.stages[s].vergleich.get("section") not in (None, "-")]
    if not ran:
        return "not measurable", "no stage ran"
    n = [(p.stages[s].vergleich["at"], f"{s}: {p.stages[s].vergleich.get('answers')}") for s in ran if p.stages[s].vergleich.get("answers", "0") != "0"]
    first = [t for s in ran for _, t in p.stages[s].has("answers of the card are not the trace's")][:1]
    # The secure OS's ready value in C2PMSG_81 is new at every start (0x0e5b5a34 Linux 18.11., 0x0a9b7218 and
    # 0x02e515c2 on the RX 9070 XT 19./20.11.); only "no longer 0" is Linux's condition, so that one is no refutation.
    m = re.search(r"MPASP_SMN_C2PMSG_81 until the value is no longer 0x00000000 \(its card answered (0x[0-9a-f]+)", first[0]) if first else None
    if n == [(p.stages["sos"].vergleich["at"], "sos: 1")] and m and int(m.group(1), 16) != 0 if "sos" in p.stages and p.stages["sos"].vergleich else False:
        return "confirmed", f"every answer is the trace's but the secure OS's ready value in C2PMSG_81 ({m.group(1)} here), which is new at every start and only has to be no longer 0, as Linux checks"
    if n:
        return "refuted", f"answers that are not the trace's: {', '.join(t for _, t in n)} ({lines(n)}); {first[0] if first else ''}"
    return "confirmed", "every read and every wait's last answer is the trace's"


def bit31(p):
    """The bootloader's waits on C2PMSG_35 in sos: (all, those that ended without bit 31)."""
    w = [(n, t) for n, t in p.stages["sos"].spur if t.startswith("WAIT MPASP_SMN_C2PMSG_35 ")]
    bad = [(n, t) for n, t in w if not re.search(r"last=0x[89a-f]", t)]
    return w, bad


def r_memory(p):
    r = row(p, "sos", "ring", "toc", "fw", "fwrest", "ta")
    if r:
        return r
    faults = ih_total(p, "faults_gfxhub") + ih_total(p, "faults_mmhub")
    w, no31 = bit31(p)
    st = check_lines(p, ["ring", "toc", "fw", "fwrest", "ta"], r"LOAD_|toc:")
    if faults:
        return "refuted", f"{faults} page faults after the GART was on"
    if not w or no31:
        return "refuted", f"{len(no31)} of the bootloader's {len(w)} waits on C2PMSG_35 ended without bit 31 ({lines(no31)})"
    return "confirmed", f"every ring command answered and its stage passed ({lines(st)}); all {len(w)} of the bootloader's waits ended with bit 31 ({lines(w)}); no fault counted in the IH"


def r_cmd_status(p):
    r = row(p, "ring", "toc", "fw", "fwrest", "ta")
    if r:
        return r
    fw = check_lines(p, ["fw", "fwrest", "ta"], r"LOAD_IP_FW")
    ta = check_lines(p, ["ta"], r"LOAD_TA \S+ .*status 0x[0-9a-f]+")
    nonzero = [(n, t) for n, t in ta if not re.search(r"status 0x0\b", t.split("RAS init")[0])]
    told = f"{len(fw)} LOAD_IP_FW lines answered (a stage stops on a status other than 0, so each was 0); LOAD_TA: " + "; ".join(re.search(r"LOAD_TA (\S+)", t).group(1) + " " + re.search(r"status (0x[0-9a-f]+)", t).group(1) for _, t in ta)
    if nonzero:
        return "confirmed", f"{told}; a status other than 0 passed over as Linux does ({lines(nonzero)})"
    return "confirmed", f"{told} ({lines(fw + ta)})"


def r_cmd_msi(p):
    r = row(p, "ih")
    if r:
        return r
    a = p.stages["ih"].has("bus master was on")
    c = p.stages["ih"].has("changed before the stage")
    if not a:
        return "refuted", "ih passed without 'bus master was on'"
    came = [(n, t) for n, t in ih_after(p, "ih") if re.search(r"ring0=\+[1-9]", t)]
    seen = f"messages arrived afterwards ({lines(came)})" if came else "no message arrived afterwards (nothing raised one before the rungs)"
    return part(f"{a[0][1][:140]}; {len(c)} 'changed before the stage' lines; {seen} ({lines(a + c)})", "the `pci` tool's command register afterwards (the run reads no configuration space after the attach)")


def r_atomics(p):
    r = row(p, "nbif")
    if r:
        return r
    a = p.stages["nbif"].has("AtomicOps to the root port granted")
    c = p.stages["nbif"].has("changed before the stage")
    if not a:
        return "refuted", "nbif passed without the AtomicOps line"
    return part(f"{a[0][1][:140]} ({lines(a + c)})", "the `pci` tool's Device Control 2 afterwards and AER errors (the run reads no configuration space after the attach)")


def r_sys_addr(p):
    r = row(p, "gmc")
    if r:
        return r
    d = p.stages["gmc"].has("dummy page")
    faults = ih_total(p, "faults_gfxhub") + ih_total(p, "faults_mmhub")
    return ("refuted", f"{faults} faults") if faults else ("confirmed", f"{d[0][1][:120] if d else ''}; no fault entry ({lines(d)})")


def r_fw_region(p):
    r = row(p, "ring", "gmc")
    if r:
        return r
    a = p.stages["ring"].has("reserved by the firmware")
    b = p.stages["gmc"].has("GART table")
    return "confirmed", f"{a[0][1][:120] if a else ''}; {b[0][1][:80] if b else ''}; the ring stage would have stopped on an overlap ({lines(a + b)})"


def r_gart_entries(p):
    if not p.ran("sdmatest"):
        return "not measurable", "no IH entry can arrive before the rungs' traps and the rungs did not run"
    t = check_lines(p, ["sdmatest"], r"rung trap-[01]: PASS")
    if len(t) == 2 and ih_total(p, "failed") == 0:
        return "confirmed", f"both traps came through the IH rings, which the card reaches only through the GART; no failed IH read ({lines(t)})"
    return "refuted", f"the traps did not both come or an IH read failed ({lines(t)})"


def r_readback(p):
    r = row(p, "gmc", "ih")
    if r:
        return r
    n = [(p.stages[s].vergleich["at"], s) for s in ("gmc", "ih") if p.stages[s].vergleich.get("answers", "0") != "0"]
    return ("refuted", f"answers differ in {', '.join(s for _, s in n)} ({lines(n)})") if n else ("confirmed", "gmc's and ih's reads, read-backs and acknowledges are the trace's")


def r_ih_unclaimed(p):
    r = row(p, "ih")
    if r:
        return r
    lst = [(n, t) for n, t in p.ihs.values() if "unclaimed=-" not in t and "unclaimed=" in t]
    if not lst:
        return "confirmed", "no entry without a handler arrived"
    bad = [x for x in lst if re.search(r"unclaimed=[^ ]*\b(10|18):0:", x[1])]
    if bad:
        return "refuted", f"a fault source (client 10 or 18, source 0) is listed as unhandled ({lines(bad)})"
    return "confirmed", f"few entries without a handler, each counted: {'; '.join(re.search(r'unclaimed=(\S+)', t).group(1) for _, t in lst)} ({lines(lst)})"


def r_ih_doorbell(p):
    st = p.look("state", "ende", r"IH_RB_CNTL")
    ses = p.look("state", "ende", r"ring 0 \d+ entries")
    if not st or not ses:
        return "not measurable", "no state look at the end with the IH on"
    m1 = re.search(r"IH_RB_CNTL \S+ RPTR (0x[0-9a-f]+)", st[0][1])
    m2 = re.search(r"ring 0 (\d+) entries \(read pointer (0x[0-9a-f]+)\)", ses[0][1])
    if not (m1 and m2):
        return "not measurable", "the state look's lines cannot be read"
    if int(m2.group(1)) == 0:
        return "not measurable", "no entry arrived, so the doorbell moved nothing"
    if int(m1.group(1), 16) == int(m2.group(2), 16):
        return "confirmed", f"IH_RB_RPTR {m1.group(1)} equals the reader's read pointer after {m2.group(1)} entries ({lines(st + ses)})"
    return "refuted", f"IH_RB_RPTR {m1.group(1)}, the reader's {m2.group(2)} ({lines(st + ses)})"


def r_ih_ring1(p):
    if ih_total(p, "self") == 0 and ih_total(p, "ring1") == 0:
        return "not measurable", "ring 1 gets GFX's entries, and GFX does not run in this chain (none arrived)"
    return "confirmed", f"self-interrupts {ih_total(p, 'self')}, ring 1 entries {ih_total(p, 'ring1')}"


def r_sos(p):
    r = row(p, "sos")
    if r:
        return r
    w = p.eval.get("waits", {}).get("sos", [])
    first = next((x for f, x in w if x["name"] == "MPASP_SMN_C2PMSG_35" and x["reads"] > 1), None)
    return "confirmed", f"the key database's wait ended with bit 31 after {first['reads'] if first else '?'} reads; every part taken; the flush before C2PMSG_36 was there (stage line {p.stages['sos'].at})"


def r_sos_vram(p):
    r = row(p, "toc", "fw", "fwrest", "ta")
    if r:
        return r
    t = p.stages["toc"].has("tmr size")
    return "confirmed", f"LOAD_TOC and every later command answered from VRAM ({lines(t)})"


def r_faults(p):
    if not p.passed("ih"):
        return "not measurable", "ih did not pass, so no fault could be counted"
    st = p.look("state", "ende", "page faults")
    if not st:
        return "not measurable", "no state look at the end shows the fault counts"
    n = ih_total(p, "faults_gfxhub") + ih_total(p, "faults_mmhub")
    clients = [(n2, t) for n2, t in ih_after(p, "ih") if re.search(r"unclaimed=[^ ]*\b(10|18):0:", t)]
    if n or clients:
        return "refuted", f"{n} page faults counted, {len(clients)} IH lines with a hub's fault unhandled ({lines(st + clients)})"
    return "confirmed", f"no page fault counted from ih to the end, and the state look at the end says so ({lines(st)})"


def r_looks(p):
    looks = [(n, t) for n, l, a, t in p.looks if re.match(r"writes=\d+ ", t)]
    wrote = [(n, t) for n, t in looks if not t.startswith("writes=0 ")]
    if wrote:
        return "refuted", f"a look wrote ({lines(wrote)})"
    return "confirmed", f"{len(looks)} looks read only (state between every two stages, alive and bootloader before, alive and state at the end; smu not run); each stage's own linuxseq line stands unaffected. Not the row's plan of a first pass without looks between nbif and ta"


def r_bl_answers(p):
    r = row(p, "sos")
    if r:
        return r
    w = [x for f, x in p.eval.get("waits", {}).get("sos", []) if x["name"] == "MPASP_SMN_C2PMSG_35" and x["reads"] > 1]
    return "confirmed", "parts' waits: " + ", ".join(f"{x['reads']} reads in {x['us']} us" for x in w) + " (Linux: 70 to 472 reads in 0.1 to 0.8 ms)"


def r_sos_20ms(p):
    r = row(p, "sos")
    if r:
        return r
    a = p.stages["sos"].has("secure OS loaded|up within the 20 ms")
    w = [x for f, x in p.eval.get("waits", {}).get("sos", []) if x["name"] == "MPASP_SMN_C2PMSG_81"]
    return "confirmed", f"{a[0][1][:100] if a else ''}; C2PMSG_81's wait {w[0]['reads'] if w else '?'} reads in {w[0]['us'] if w else '?'} us (Linux 5131 in 8586 us) ({lines(a)})"


def r_c2pmsg64(p):
    r = row(p, "ring")
    if r:
        return r
    w = p.stages["ring"].has("C2PMSG_64 until")
    vals = [re.search(r"last (0x[0-9a-f]+)", t).group(1) for _, t in w]
    ok = vals[:2] == ["0x80c00000", "0x80c20000"]
    return ("confirmed" if ok else "refuted"), f"ring's waits ended at {', '.join(vals)} ({lines(w)}); destroy's half is not measurable (destroy does not run)"


def r_reserved(p):
    r = row(p, "ring")
    if r:
        return r
    a = p.stages["ring"].has("reserved by the firmware")
    m = re.search(r"(0x[0-9a-f]+) bytes at (0x[0-9a-f]+), extended (0x[0-9a-f]+) bytes at (0x[0-9a-f]+)", a[0][1]) if a else None
    if not m:
        return "not measurable", "no reserved-region line"
    # Where the 4 MiB lie is the card's own answer (Linux's trace does not say; on 19.11. the RX 9070 XT gave
    # 0x3dac00000, and Linux's GART table of 18.11. sits right below that), so only the sizes are judged.
    ok = int(m.group(1), 16) == 50 << 20 and int(m.group(3), 16) == 4 << 20
    return ("confirmed" if ok else "refuted"), f"{a[0][1][:140]} (Linux: 50 MiB at the top and 4 MiB more) ({lines(a)})"


def r_cmd_time(p):
    r = row(p, "ring", "toc", "fw", "fwrest", "ta")
    if r:
        return r
    took = [(s, p.stages[s].zeit) for s in ("ring", "toc", "fw", "autoload", "fwrest", "ta") if p.ran(s)]
    t = ", ".join(f"{s} {us} us" for s, us in took)
    long = [s for s, us in took if us is None or us >= 2_000_000]
    if not long:
        return "confirmed", f"each stage took less than 2 s in all, so each of its fences came within 2 s: {t}"
    return part(f"the stages' run times: {t}", f"the time of each fence on its own ({', '.join(long)} took 2 s or more in all; the protocol does not time a single command)")


CHECKS = {
    "accesses": r_accesses,
    "wait-time": r_wait_time,
    "answers": r_answers,
    "memory": r_memory,
    "atomics": r_atomics,
    "cmd-msi": r_cmd_msi,
    "sys-addr": r_sys_addr,
    "fw-region": r_fw_region,
    "gart-entries": r_gart_entries,
    "readback-bits": r_readback,
    "ih-unclaimed": r_ih_unclaimed,
    "ih-doorbell": r_ih_doorbell,
    "ih-ring1": r_ih_ring1,
    "reload-ih": nm("needs a Reload with the IH on; the run's quiesce hooks run at the power-off, after the protocol's last line"),
    "bl-vram": r_sos,
    "bl-hdp": lambda p: r_sos(p) if r_sos(p)[0] != "confirmed" else ("confirmed", "with the flush the loads passed; whether they pass without it is not tried"),
    "sos-vram": r_sos_vram,
    "reload-again": nm("needs a Reload after the chain; the run switches the PC off instead"),
    "rundcbtc": r_rundcbtc,
    "pcie-override": r_pcie,
    "pptlimit": r_pptlimit,
    "gc-before-power": r_gcboot(r"the autoload is complete"),
    "autoload-loop": r_gcboot(r"at round \d+ of Linux's loop"),
    "bootload-value": r_bootload_value,
    "gc-starts": r_gcboot(r"the engines will start at"),
    "grbm": r_grbm,
    "gc-pulses": r_pulses,
    "gchub-reads": r_gchub,
    "flush-both": r_flush_both,
    "faults": r_faults,
    "reload-gchub": nm("needs a Reload after gchub"),
    "second-fw": nm("not tried on purpose: a second fw needs destroy and a new ring"),
    "looks": r_looks,
    "bl-answers": r_bl_answers,
    "sos-20ms": r_sos_20ms,
    "second-sos": nm("not tried on purpose (a second sos after a partial load)"),
    "c2pmsg64": r_c2pmsg64,
    "reserved": r_reserved,
    "tmr-size": r_simple("toc", r"tmr size"),
    "cmd-status": r_cmd_status,
    "cmd-time": r_cmd_time,
    "picture": nm("only Georg's eyes see the picture during fw and fwrest: ask him"),
    "ras-flags": r_simple("ta", r"LOAD_TA RAS .*status 0x[0-9a-f]+, RAS init status"),
    "ta-buffers": r_simple("ta", r"LOAD_TA DTM"),
    "training": nm("nothing to check: memory training is never sent"),
    "vcn-place": r_simple("fwrest", r"LOAD_IP_FW Vcn .*placed at"),
    "pmfw-answers": r_pmfw_answers,
    "enable-time": r_enable_time,
    "scpm": r_scpm,
    "fw-flags": r_simple("pmfw", r"smu: firmware flags 0x[0-9a-f]*[13579bdf] at SMN"),
    "c2pmsg90": nm("needs the look smu between fw and pmfw, which sends questions Linux's start does not send; not run"),
    "link-limits": r_link,
    "pmfw-tables": r_simple("pmfw", r"tables in vram"),
    "pptable-layout": r_layout,
    "video-power": r_video,
    "gc-power": nm("nothing to check while every GC access follows pmfw (the chain's order)"),
    "pmfw-reload": nm("not tried on purpose: the PC is switched off after the run"),
    "stop-features": nm("needs a Reload or a stop with the features on; the run switches the PC off"),
    "sdma-doorbell": r_simple("sdma", r"ring test passed after \d+ reads"),
    "sdma-wptr": r_simple("sdma", r"sdma1: .*ring test passed"),
    "sdma-packets": r_rungs(r"rung copy-sys-to-vram: PASS", "every rung passed"),
    "fill-form": r_fill_form,
    "trap": r_rungs(r"rung trap-1: PASS", "trap-0 and trap-1"),
    "fence-wait": r_rungs(r"after \d+ polls", "the fences"),
    "sdma-fw": r_simple("sdma", r"sdma0: .*ring test passed"),
    "sdma-stopped": r_after_failure(r"sdma registers", "the engines' registers after a failure"),
    "gfxhub-status": nm("needs a fault of a copy engine; a provoked fault is not part of the first round"),
    "irq-in-rungs": r_irq,
    "vram-engines": r_rungs(r"rung copy-vram: PASS", "copy-vram"),
    "reload-sdma": nm("needs a Reload after sdma"),
    "rung-kept": r_after_failure(r"kept after the failed rung", "what a failed rung keeps"),
    **CP_CHECKS,
    **MES_CHECKS,
    **RING_CHECKS,
    **THERMAL,
}
