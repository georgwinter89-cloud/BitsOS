#!/usr/bin/env python3
"""CP-3 scenario check (V1 apic-test.mjs, monitor part): QEMU's own view of the interrupt controllers against
the MADT and the kernel's events.

usage: apic_check.py SERIAL_LOG MONITOR_TXT
SERIAL_LOG: the kernel's COM1 log (acpi madt.override / acpi fadt / apic setup / irq route events).
MONITOR_TXT: qmp_hmp.py output of 'info lapic' and 'info pic'.
Prints one ok/FAIL line per check; exit status 1 if any failed.
"""
import re
import sys

fails = 0


def check(cond, what):
    global fails
    print(("ok   " if cond else "FAIL ") + what)
    if not cond:
        fails += 1


def main():
    serial = open(sys.argv[1], encoding="utf-8", errors="replace").read()
    mon = open(sys.argv[2], encoding="utf-8", errors="replace").read()

    overrides = {int(m[0]): (int(m[1]), int(m[2], 16)) for m in re.findall(r"acpi madt\.override bus=0 irq=(\d+) gsi=(\d+) flags=0x([0-9a-f]+)", serial)}
    sci = re.search(r"acpi fadt .*?sci_int=(\d+)", serial)
    setup = re.search(r"apic setup (.*)", serial)
    routes = [
        dict(purpose=m[0], source=int(m[1]), gsi=int(m[2]), pin=int(m[4]), vector=int(m[5], 16), level=m[6] == "level", low=m[7] == "low", dest=int(m[8]))
        for m in re.findall(
            r"irq route purpose=(\S+) source=(\d+) gsi=(\d+) ioapic=(\d+) pin=(\d+) vector=0x([0-9a-f]+) trigger=(\w+) polarity=(\w+) dest=(\d+)", serial
        )
    ]
    check(setup is not None, "kernel logged 'apic setup'")
    check(sci is not None, "kernel logged the FADT's SCI_INT")
    check(any(r["purpose"] == "sci" for r in routes), "the SCI is routed")
    if sci:
        for r in routes:
            if r["purpose"] == "sci":
                check(r["source"] == int(sci[1]), f"SCI route source {r['source']} == FADT SCI_INT {sci[1]}")

    # 1. The kernel's routes follow the MADT (override flags; ISA defaults edge/high, SCI level/low).
    for r in routes:
        sci_route = r["purpose"] == "sci"
        gsi, flags = overrides.get(r["source"], (r["source"], 0))
        pol, trig = flags & 3, (flags >> 2) & 3
        want_low = {1: False, 3: True}.get(pol, sci_route)
        want_level = {1: False, 3: True}.get(trig, sci_route)
        check(r["gsi"] == gsi, f"{r['purpose']}: gsi {r['gsi']} == MADT {gsi}")
        check((r["level"], r["low"]) == (want_level, want_low), f"{r['purpose']}: level={r['level']} low={r['low']} as MADT flags {flags:#x} say")

    # 2. QEMU's I/O APIC holds exactly those routes (masked until an owner installs a handler); every other pin
    # is masked.
    pins = {}
    for m in re.finditer(r"^\s*pin (\d+)\s+0x([0-9a-f]+) dest=(\d+) vec=(\d+)\s+(active-hi|active-lo) (edge|level)\s+(masked)?", mon, re.M):
        pins[int(m[1])] = dict(dest=int(m[3]), vec=int(m[4]), low=m[5] == "active-lo", level=m[6] == "level", masked=m[7] is not None)
    check(len(pins) > 0, f"info pic lists I/O APIC pins ({len(pins)})")
    routed = {r["pin"]: r for r in routes}
    # A routed pin is unmasked only while its owner has a handler installed (the last handler.set event).
    owned = {}
    for m in re.finditer(r"irq handler\.set purpose=(\S+) installed=(true|false)", serial):
        owned[m[1]] = m[2] == "true"
    stormed = set(re.findall(r"irq storm purpose=(\S+)", serial))
    for pin, p in sorted(pins.items()):
        r = routed.get(pin)
        if r is None:
            if not p["masked"]:
                check(False, f"pin {pin} is unmasked but not routed")
            continue
        want_masked = not owned.get(r["purpose"], False) or r["purpose"] in stormed
        check(p["masked"] == want_masked, f"pin {pin} ({r['purpose']}) masked={p['masked']}, expected {want_masked} (owner handler: {owned.get(r['purpose'], False)})")
        check(p["vec"] == r["vector"], f"pin {pin} vector {p['vec']:#x} == {r['vector']:#x}")
        check((p["level"], p["low"]) == (r["level"], r["low"]), f"pin {pin} trigger/polarity as routed")
        check(p["dest"] == r["dest"], f"pin {pin} dest {p['dest']} == BSP {r['dest']}")
    unmasked_other = [pin for pin, p in pins.items() if not p["masked"] and pin not in routed]
    check(not unmasked_other, f"no other unmasked pin ({unmasked_other})")

    # 3. The LAPIC as QEMU sees it.
    nmi = int(re.search(r"nmi_lint=(\d)", setup[1])[1]) if setup else 1
    check(re.search(r"^SPIV\s+0x000001ff", mon, re.M) is not None, "SPIV 0x1ff (enabled, spurious vector 255)")
    lvt = {m[0]: m[1] for m in re.findall(r"^(LVT0|LVT1|LVTT)\s+0x[0-9a-f]+ (.*)$", mon, re.M)}
    other = "LVT0" if nmi == 1 else "LVT1"
    check("masked" in lvt.get(other, ""), f"{other} masked ({lvt.get(other)!r})")
    nmi_lvt = lvt.get(f"LVT{nmi}", "")
    check("NMI" in nmi_lvt and "masked" not in nmi_lvt, f"LVT{nmi} = NMI, not masked ({nmi_lvt!r})")
    check("masked" in lvt.get("LVTT", "") and "vec 240" in lvt.get("LVTT", ""), f"LVT timer masked, vector 0xF0 ({lvt.get('LVTT')!r})")
    check(re.search(r"^ISR\s+\(none\)", mon, re.M) is not None, "nothing left in service")

    # 4. Both 8259s masked, bases 0x20/0x28.
    for name, base in (("pic0", "20"), ("pic1", "28")):
        m = re.search(rf"^{name}: .*imr=([0-9a-f]+) .*irq_base=([0-9a-f]+)", mon, re.M)
        check(m is not None and m[1] == "ff" and m[2] == base, f"{name} imr=ff irq_base={base}")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
