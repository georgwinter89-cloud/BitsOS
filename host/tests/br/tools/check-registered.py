#!/usr/bin/env python3
"""BR-1: tie the forecast's "not registered in V2" list to V2's syscall registrations.

usage: check-registered.py ROOT
Reads the syscall names Brave called (the "syscalls by count" table of host/tests/br/ref/br1-{local,net}.txt) and
every registration in crates/*/src (syscall!, syscall_nr!, fs_call!, sock_call!, both the uapi-name and the
number + "name" forms). Fails (exit 1) when a name Brave calls is unregistered but not in EXPECTED (a registration
the notes rely on went missing), or when the scan finds implausibly few registrations (the pattern broke).
A name of EXPECTED that is registered by now only prints a hint to update plan/notes/br-BR-1.md.
"""
import pathlib
import re
import sys

# plan/notes/br-BR-1.md, "Not registered in V2" (fgetxattr/fsetxattr only in the net run).
# pkey_alloc and pkey_mprotect are registered since INT-W10 (BR-3 CR 4).
EXPECTED = {"rseq", "landlock_create_ruleset", "fgetxattr", "fsetxattr"}
REFS = ("br1-local.txt", "br1-net.txt")
BY_NAME = re.compile(r"\b(?:syscall|fs_call|sock_call)!\(\s*([A-Z][A-Za-z0-9_]*)\s*,")
BY_NR = re.compile(r"\b(?:syscall_nr|fs_call|sock_call)!\(\s*[^,()]+,\s*\"([a-z0-9_]+)\"")


def registered(root):
    names = set()
    for f in (root / "crates").glob("**/src/**/*.rs"):
        text = f.read_text(errors="replace")
        names.update(m.lower() for m in BY_NAME.findall(text))
        names.update(BY_NR.findall(text))
    return names


def used(ref):
    names, on = set(), False
    for line in ref.read_text().splitlines():
        if line.startswith("## "):
            on = line == "## syscalls by count"
            continue
        parts = line.split()
        if on and len(parts) == 2 and parts[0].isdigit():
            names.add(parts[1])
    return names


def main():
    root = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else ".").resolve()
    reg = registered(root)
    bad = False
    if len(reg) < 150:
        print(f"FAIL: only {len(reg)} registrations found; the scan pattern no longer matches the macros")
        return 1
    missing_all = set()
    for name in REFS:
        calls = used(root / "host/tests/br/ref" / name)
        if not calls:
            print(f"FAIL: no syscall table in {name}")
            return 1
        missing = calls - reg
        missing_all |= missing
        print(f"{name}: {len(calls)} syscalls, unregistered: {' '.join(sorted(missing)) or '-'}")
        for n in sorted(missing - EXPECTED):
            print(f"FAIL: {n} is called by Brave and the notes count it as registered, but no registration found")
            bad = True
    for n in sorted(EXPECTED - missing_all):
        print(f"note: {n} is registered now; update the forecast in plan/notes/br-BR-1.md")
    print(f"{len(reg)} registered names scanned")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
