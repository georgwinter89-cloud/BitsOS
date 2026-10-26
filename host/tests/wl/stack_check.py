#!/usr/bin/env python3
"""WL: the rtw89 driver thread's stack, measured in a built kernel (static, from its machine code).

The thread runs on a kernel stack of 64 KiB (crates/sched/src/stack.rs). At opt-level 1 (the dev profile the
stick runs) a Device moved by value through four bring-up frames overflowed it on the Mini-PC with wlan=on
(`thread 38 "rtw89" ran into its guard page`, rip in Device::bring_up's stack probe). QEMU has no RTL8852BE, so no
ktest ever enters these frames, and the fake chip of the host tests is a node process: the kernel's machine code is
the only place the frames can be seen without the chip.

What it measures, per function: the return address, the pushes and the `sub $n,%rsp` of the prologue, the 4 KiB
stack-probe pages included (unrolled or as LLVM's probe loop over %r11). Then the direct calls (call and tail jmp
to a symbol) from `rtw89::card::run` (the thread's body), through every function of the rtw89, rtw89_core and wifi
crates (their monomorphised generics included); a function of another crate counts with its own frame and ends the
path there (sched, klog, mm and the other kernel crates are measured by their owners). Recursion inside the driver
(the stable sort's quicksort) counts every member of the cycle once. Calls through `dyn` (the driver's hook traits,
StaEnv, RxOps) are not in the machine code's call targets: the deepest implementation of a driver trait is added on
top of the deepest direct path, as if it ran at its very end.

Budgets (WL-4 stack fix, 2026-10-22): the deepest path plus the deepest hook at most 24 KiB; no driver function
above 8 KiB. Before the fix: card::run 25.9 KiB, Runner::bring_up 23.4 KiB, Device::bring_up 17.9 KiB and
Device::up 23.8 KiB frames, a path of more than 100 KiB.

usage: stack_check.py KERNEL [--budget-path BYTES] [--budget-frame BYTES] [--objdump PATH]
       stack_check.py --selftest
Prints the path and the largest frames; exit status 1 above a budget.
"""
import collections
import os
import re
import subprocess
import sys

ROOT = "rtw89::card::run"
BUDGET_PATH = 24 * 1024
BUDGET_FRAME = 8 * 1024
PROLOGUE_MAX = 40  # instructions looked at before the first call

HDR = re.compile(r"^([0-9a-f]{16}) <(.*)>:$")
INS = re.compile(r"^\s*[0-9a-f]+:\s+(.*)$")
TARGET = re.compile(r"^(?:0x)?[0-9a-f]+ <(.*)>$")
HOOK = re.compile(r" as (rtw89_core|rtw89|wifi)::")


def own(name):
    return "rtw89" in name or name.startswith("wifi::") or name.startswith("<wifi::")


def parse(lines):
    """frame bytes and direct call targets per function, from GNU or LLVM objdump -d -C --no-show-raw-insn"""
    frame = {}
    calls = collections.defaultdict(set)
    cur = None
    n = 0
    scanning = False
    loop = None  # None, then the %r11 bound of LLVM's probe loop, then "done"
    for line in lines:
        m = HDR.match(line)
        if m:
            cur = m.group(2)
            frame[cur] = 8  # the return address
            n, scanning, loop = 0, True, None
            continue
        m = INS.match(line)
        if cur is None or not m:
            continue
        parts = m.group(1).strip().split(None, 1)
        if not parts:
            continue
        op = parts[0]
        raw = parts[1].strip() if len(parts) > 1 else ""
        args = raw.split("#")[0].strip().replace(", ", ",")
        n += 1
        if scanning:
            imm = int(args[1:].split(",")[0], 16) if args.startswith("$0x") else None
            if op.startswith("push"):
                frame[cur] += 8
            elif op.startswith("sub") and args.endswith(",%rsp") and imm is not None:
                if not (isinstance(loop, int) and imm == 0x1000):  # a page of the loop: counted by its bound
                    frame[cur] += imm
            elif op.startswith("mov") and args == "%rsp,%r11":
                loop = 0
            elif op.startswith("sub") and args.endswith(",%r11") and loop == 0 and imm is not None:
                loop = imm
            elif op.startswith("cmp") and args == "%r11,%rsp" and isinstance(loop, int):
                frame[cur] += loop
                loop = "done"
            elif op.startswith("call") or op.startswith("ret") or n > PROLOGUE_MAX:
                scanning = False
        if op.startswith("call") or op.startswith("jmp"):
            t = TARGET.match(raw)
            if t and "+0x" not in t.group(1) and t.group(1) != cur:
                calls[cur].add(t.group(1))
    return frame, calls


def deepest(frame, calls, roots):
    """longest path (bytes) from each of `roots` over the driver's graph: other crates are leaves, a cycle's
    members count once each (Tarjan's components, then the longest path over the acyclic rest)"""
    def succ(f):
        return sorted(calls.get(f, ())) if own(f) else []

    index, low, onst, stack, comp, comps = {}, {}, set(), [], {}, []
    counter = 0
    for r in roots:
        if r in index or r not in frame:
            continue
        index[r] = low[r] = counter
        counter += 1
        stack.append(r)
        onst.add(r)
        work = [(r, iter(succ(r)))]
        while work:
            v, it = work[-1]
            pushed = False
            for w in it:
                if w not in index:
                    index[w] = low[w] = counter
                    counter += 1
                    stack.append(w)
                    onst.add(w)
                    work.append((w, iter(succ(w))))
                    pushed = True
                    break
                if w in onst:
                    low[v] = min(low[v], index[w])
            if pushed:
                continue
            work.pop()
            if work:
                u = work[-1][0]
                low[u] = min(low[u], low[v])
            if low[v] == index[v]:
                c = []
                while True:
                    w = stack.pop()
                    onst.discard(w)
                    comp[w] = len(comps)
                    c.append(w)
                    if w == v:
                        break
                comps.append(c)
    best = [0] * len(comps)
    nxt = [None] * len(comps)
    for i, c in enumerate(comps):  # Tarjan emits a component after everything it reaches
        b, bn = 0, None
        for x in c:
            for w in succ(x):
                j = comp[w]
                if j != i and best[j] > b:
                    b, bn = best[j], j
        best[i] = sum(frame.get(x, 0) for x in c) + b
        nxt[i] = bn
    out = {}
    for r in roots:
        if r not in comp:
            continue
        path, i = [], comp[r]
        while i is not None:
            path.append(comps[i])
            i = nxt[i]
        out[r] = (best[comp[r]], path)
    return out, set(comp)


def objdump_cmd(kernel, tool):
    if tool:
        return [tool, "-d", "-C", "--no-show-raw-insn", kernel]
    try:
        sysroot = subprocess.run(["rustc", "--print", "sysroot"], capture_output=True, text=True, check=True).stdout.strip()
        host = re.search(r"^host: (\S+)", subprocess.run(["rustc", "-vV"], capture_output=True, text=True, check=True).stdout, re.M).group(1)
        llvm = os.path.join(sysroot, "lib", "rustlib", host, "bin", "llvm-objdump")
        if os.path.isfile(llvm):
            return [llvm, "-d", "-C", "--no-show-raw-insn", kernel]
    except (OSError, subprocess.CalledProcessError, AttributeError):
        pass
    return ["objdump", "-d", "-C", "--no-show-raw-insn", kernel]


def short(s, w=150):
    return s if len(s) <= w else s[: w - 3] + "..."


def check(frame, calls, budget_path, budget_frame, report=print):
    if ROOT not in frame:
        report(f"FAIL {ROOT} is not in the kernel (renamed? then this check's ROOT)")
        return False
    run, reach = deepest(frame, calls, [ROOT])
    total, path = run[ROOT]
    # the hooks: driver trait implementations (called through dyn) that do not lead back to the thread's body
    hooks = [f for f in frame if own(f) and HOOK.search(f)]
    hd, hreach = deepest(frame, calls, hooks)
    hd = {h: v for h, v in hd.items() if not any(ROOT in c for comp in v[1] for c in comp)}
    hook, (hbytes, hpath) = max(hd.items(), key=lambda kv: kv[1][0], default=(None, (0, [])))
    report(f"deepest direct path from {ROOT}: {total} bytes ({total / 1024:.1f} KiB), {len(path)} frames")
    acc = 0
    for c in path:
        w = sum(frame.get(x, 0) for x in c)
        acc += w
        more = f"  (+{len(c) - 1} in a cycle)" if len(c) > 1 else ""
        report(f"  {w:7d} {acc:7d}  {short(c[0])}{more}")
    report(f"deepest driver hook (through dyn): {hbytes} bytes" + (f", {short(hook)}" if hook else ""))
    worst = total + hbytes
    ok = True
    line = f"path + hook = {worst} bytes ({worst / 1024:.1f} KiB), budget {budget_path} ({budget_path / 1024:.1f} KiB)"
    if worst > budget_path:
        report("FAIL " + line)
        ok = False
    else:
        report("ok   " + line)
    mine = sorted(((frame.get(f, 0), f) for f in (reach | hreach) if own(f)), reverse=True)
    report("largest driver frames:")
    for b, f in mine[:15]:
        report(f"  {b:7d}  {short(f)}")
    big = [(b, f) for b, f in mine if b > budget_frame]
    for b, f in big:
        report(f"FAIL frame {b} bytes above {budget_frame}: {short(f)}")
    if not big:
        report(f"ok   no driver frame above {budget_frame} bytes (largest {mine[0][0] if mine else 0})")
    return ok and not big


GNU = """\
ffffffff80001000 <rtw89::card::run>:
ffffffff80001000:\tpush   %rbp
ffffffff80001001:\tmov    %rsp,%rbp
ffffffff80001004:\tsub    $0x100,%rsp
ffffffff8000100b:\tcall   ffffffff80002000 <rtw89_core::big>
ffffffff80001010:\tcall   *%rax
ffffffff80001012:\tjmp    ffffffff80001000 <rtw89::card::run+0x0>
ffffffff80001014:\tjmp    ffffffff80004000 <klog::emit>

ffffffff80002000 <rtw89_core::big>:
ffffffff80002000:\tpush   %rbp
ffffffff80002001:\tmov    %rsp,%rbp
ffffffff80002004:\tpush   %rbx
ffffffff80002005:\tsub    $0x1000,%rsp
ffffffff8000200c:\tmovq   $0x0,(%rsp)
ffffffff80002014:\tsub    $0x1000,%rsp
ffffffff8000201b:\tmovq   $0x0,(%rsp)
ffffffff80002023:\tsub    $0x658,%rsp
ffffffff8000202a:\tcall   ffffffff80003000 <rtw89_core::sort::<u32>>

ffffffff80003000 <rtw89_core::sort::<u32>>:
ffffffff80003000:\tpush   %rbp
ffffffff80003001:\tsub    $0x20,%rsp
ffffffff80003005:\tcall   ffffffff80003000 <rtw89_core::sort::<u32>>
ffffffff8000300a:\tcall   ffffffff80003100 <rtw89_core::merge::<u32>>

ffffffff80003100 <rtw89_core::merge::<u32>>:
ffffffff80003100:\tpush   %rbp
ffffffff80003101:\tcall   ffffffff80003000 <rtw89_core::sort::<u32>>
ffffffff80003106:\tret

ffffffff80004000 <klog::emit>:
ffffffff80004000:\tpush   %rbp
ffffffff80004001:\tsub    $0x500,%rsp
ffffffff80004008:\tcall   ffffffff80005000 <klog::deep>
"""

LLVM = """\
ffffffff80006000 <<rtw89_core::Hook as rtw89_core::phy::ChipOps<rtw89_core::Bus>>::rfk>:
ffffffff80006000:      \tpushq\t%rbp
ffffffff80006001:      \tmovq\t%rsp, %rbp
ffffffff80006004:      \tmovq\t%rsp, %r11
ffffffff80006007:      \tsubq\t$0x3000, %r11       # imm = 0x3000
ffffffff8000600e:      \tsubq\t$0x1000, %rsp       # imm = 0x1000
ffffffff80006015:      \tmovq\t$0x0, (%rsp)
ffffffff8000601d:      \tcmpq\t%r11, %rsp
ffffffff80006020:      \tjne\t0xffffffff8000600e <<rtw89_core::Hook as rtw89_core::phy::ChipOps<rtw89_core::Bus>>::rfk+0xe>
ffffffff80006022:      \tsubq\t$0x1000, %rsp       # imm = 0x1000
ffffffff80006029:      \tcallq\t0xffffffff80007000 <<alloc::vec::Vec<u8> as core::clone::Clone, x>::clone>
"""


def selftest():
    frame, calls = parse((GNU + LLVM).splitlines())
    expect = {
        "rtw89::card::run": 8 + 8 + 0x100,
        "rtw89_core::big": 8 + 16 + 0x2658,
        "rtw89_core::sort::<u32>": 8 + 8 + 0x20,
        "rtw89_core::merge::<u32>": 16,
        "klog::emit": 8 + 8 + 0x500,
        "<rtw89_core::Hook as rtw89_core::phy::ChipOps<rtw89_core::Bus>>::rfk": 8 + 8 + 0x3000 + 0x1000,
    }
    bad = [f"{k}: {frame.get(k)} != {v}" for k, v in expect.items() if frame.get(k) != v]
    if calls["rtw89::card::run"] != {"rtw89_core::big", "klog::emit"}:
        bad.append(f"calls of run: {calls['rtw89::card::run']}")
    if "<alloc::vec::Vec<u8> as core::clone::Clone, x>::clone" not in calls[next(k for k in expect if "Hook" in k)]:
        bad.append("an LLVM call target with ', ' in its name was cut")
    run, _ = deepest(frame, calls, [ROOT])
    # run -> big -> {sort, merge} (a cycle, both once); klog::emit ends its path (another crate)
    want = expect["rtw89::card::run"] + expect["rtw89_core::big"] + expect["rtw89_core::sort::<u32>"] + 16
    if run[ROOT][0] != want:
        bad.append(f"deepest {run[ROOT][0]} != {want}")
    lines = []
    ok = check(frame, calls, want + expect["<rtw89_core::Hook as rtw89_core::phy::ChipOps<rtw89_core::Bus>>::rfk"], 0x4010, lines.append)
    if not ok:
        bad.append("budgets exactly met must pass: " + " | ".join(lines))
    if check(frame, calls, want, 1 << 20, lambda _: None):
        bad.append("the hook's depth must count against the path budget")
    if check(frame, calls, 1 << 20, 0x2000, lambda _: None):
        bad.append("a frame above the frame budget must fail")
    for b in bad:
        print("FAIL selftest: " + b)
    print("selftest " + ("RED" if bad else "ok"))
    return not bad


def main():
    a = sys.argv[1:]
    if a == ["--selftest"]:
        sys.exit(0 if selftest() else 1)
    budget_path, budget_frame, tool, kernel = BUDGET_PATH, BUDGET_FRAME, None, None
    while a:
        x = a.pop(0)
        if x == "--budget-path":
            budget_path = int(a.pop(0))
        elif x == "--budget-frame":
            budget_frame = int(a.pop(0))
        elif x == "--objdump":
            tool = a.pop(0)
        elif kernel is None:
            kernel = x
        else:
            sys.exit(__doc__)
    if kernel is None:
        sys.exit(__doc__)
    cmd = objdump_cmd(kernel, tool)
    print("disassembling with " + cmd[0])
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, text=True, errors="replace")
    frame, calls = parse(p.stdout)
    if p.wait() != 0:
        sys.exit(f"{cmd[0]} failed ({p.returncode})")
    sys.exit(0 if check(frame, calls, budget_path, budget_frame) else 1)


if __name__ == "__main__":
    main()
