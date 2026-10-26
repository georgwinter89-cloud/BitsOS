"""Builds V2 items with the Agent SDK on Georg's monthly API credit (the Max plan's credit pays the Agent SDK,
not Claude Code), for weeks when the subscription limit is used up.

Same briefs as the desk's workflow (extract.mjs). Per item: builder (Opus) -> one read-only checker (Sonnet;
Opus for trusted-base items) -> fixer (Opus). Items a workflow already finished are skipped; a half-done
worktree is continued. Stops before the total budget would be passed.

    .venv/bin/python runner.py [--budget 190] [--parallel 2] [--only CP-5,RB-2] [--dry]
"""

import argparse
import asyncio
import json
import os
import subprocess
import sys
from datetime import datetime
from pathlib import Path

from claude_agent_sdk import AssistantMessage, ClaudeAgentOptions, ResultMessage, TextBlock, query

HERE = Path(__file__).resolve().parent
V2 = HERE.parent.parent
KEY_FILE = Path.home() / ".config/bitsos/anthropic-api-key"
OUT = HERE / "out"
# Items wait for these to be built first (same rule as the workflow's gates).
NEEDS = {"MC-4a": ["FS-1"], "UL-2": ["FS-2"], "AB-3": ["FS-2"], "IN-3": ["IN-2a"], "RB-3": ["RB-2"], "NW-4": ["PC-3"]}
# Agent budgets in USD; the run refuses to start an agent that could pass the total.
COST = {"build": 40.0, "check": 8.0, "fix": 25.0}
# Shell commands the agents may run without asking (build, test, read the repo). No sudo, no rm -rf, no curl.
SHELL = ["cargo", "git status", "git log", "git diff", "git show", "git add", "git commit", "git merge", "git branch",
         "git stash", "git -C", "git rev-parse", "git merge-base", "ls", "cat", "head", "tail", "wc", "grep", "rg", "find", "sed -n", "diff", "sort", "uniq",
         "python3", "node", "nasm", "objdump", "readelf", "nm", "qemu-img", "mkdir", "cp", "mv", "touch", "kill",
         "pgrep", "ps", "sleep", "timeout", "bash host/tests", "sh host/tests", "./host/tests", "rustc", "rustup",
         "musl-gcc", "cc", "clang", "make", "xxd", "od", "cmp", "sha256sum", "true", "echo", "test", "pwd", "which"]

spent = 0.0
reserved = 0.0  # budgets of agents still running (parallel chains)
stop = False


def log(msg):
    line = f"{datetime.now():%H:%M:%S} {msg}"
    print(line, flush=True)
    with open(OUT / "runner.log", "a") as f:
        f.write(line + "\n")


def git(wt, *args):
    return subprocess.run(["git", "-C", wt, *args], capture_output=True, text=True).stdout


def state(it):
    """'new' (no commit for the item on its lane), 'built' (committed, no fixer pass in its notes), 'done'."""
    commits = git(it["wt"], "log", "--oneline", "-F", "--grep", it["id"], "main..HEAD")
    notes = [p for p in (Path(it["wt"]) / "plan/notes").glob("*.md") if it["id"] in p.name]
    # A builder may name its commit differently ("MM release fix"); a committed notes file counts too.
    committed = any(git(it["wt"], "log", "--oneline", "-1", "main..HEAD", "--", str(p)).strip() for p in notes)
    if not commits.strip() and not committed:
        return "new"
    if any("ixer" in p.read_text(errors="replace") for p in notes):
        return "done"
    return "built"


async def run_agent(role, it, prompt, model, effort, read_only=False):
    global spent, reserved, stop
    cap = it.get("cost", {}).get(role, COST[role])
    # Running agents rarely use their whole cap: wait for them to end before giving up on the budget.
    while not stop and reserved > 0 and spent + reserved + cap > ARGS.budget and spent + cap <= ARGS.budget:
        await asyncio.sleep(10)
    # Rather a smaller cap than no agent: the agent stops by itself at max_budget_usd.
    left = ARGS.budget - spent - reserved
    if left >= 5 and cap > left:
        log(f"{it['id']} {role}: cap cut from {cap:.0f} $ to the {left:.2f} $ left")
        cap = left
    if stop or spent + reserved + cap > ARGS.budget:
        stop = True
        log(f"{it['id']} {role}: not started, budget {ARGS.budget:.0f} $ would be passed (spent {spent:.2f} $)")
        return None
    reserved += cap
    log(f"{it['id']} {role}: start ({model}, {effort})")
    # No prompts can be answered here: whatever is not allowed below is refused. Edits only inside the worktree
    # (acceptEdits keeps them to cwd); shell only for the build and test commands the items need.
    tools = ["Read", "Glob", "Grep"] + [f"Bash({c}:*)" for c in SHELL]
    opts = ClaudeAgentOptions(
        cwd=it["wt"],
        model=model,
        effort=effort,
        # Checkers too: in "default" mode the allowed cargo commands were still refused (15.10. run); Edit/Write stay
        # disallowed for them below.
        permission_mode="acceptEdits",
        allowed_tools=tools if read_only else tools + ["Edit", "Write"],
        disallowed_tools=["Edit", "Write", "NotebookEdit", "WebFetch", "WebSearch"] if read_only else ["NotebookEdit", "WebFetch", "WebSearch"],
        max_budget_usd=cap,
        env={"ANTHROPIC_API_KEY": KEY, "CLAUDE_CONFIG_DIR": str(HERE / ".claude-home")},
    )
    text, cost = [], 0.0
    try:
        async for m in query(prompt=prompt, options=opts):
            if isinstance(m, AssistantMessage):
                text += [b.text for b in m.content if isinstance(b, TextBlock)]
            elif isinstance(m, ResultMessage):
                cost = m.total_cost_usd or 0.0
                if m.is_error:
                    log(f"{it['id']} {role}: ended with error {m.subtype} {m.errors or ''}")
    except Exception as e:  # noqa: BLE001 - one failed agent must not end the other chains
        log(f"{it['id']} {role}: failed: {e}")
        if "credit balance" in str(e).lower():
            stop = True
    reserved -= cap
    spent += cost
    report = "\n".join(text[-3:])
    (OUT / f"{it['id']}-{role}.md").write_text(report)
    log(f"{it['id']} {role}: {cost:.2f} $ (total {spent:.2f} $)")
    return report


async def run_item(it):
    head = f"{DATA['COMMON']}\n\nYour lane: {it['lane']}. Worktree: {it['wt']} (branch lane/{it['lane']}). Port range: {it['ports']}.\n"
    s = state(it)
    log(f"{it['id']}: {s}")
    built = ""
    if s == "new":
        built = await run_agent("build", it, head + "\nYour item:\n" + it["brief"] +
            "\n\nA builder of an earlier run may have been interrupted in this worktree: look at git status and git log first and continue from that state (keep good work, finish or redo the rest)."
            "\n\nEnd with a short report: summary, files, how you tested and the results, change requests for the desk (exact), open questions with your recommendation, and whether the brief is fully met.",
            "opus", "high")
        if built is None or state(it) == "new":
            log(f"{it['id']}: builder left no commit, stopping this chain")
            return False
    elif s == "done":
        return True
    if it.get("no_review"):  # small integration rounds: the gate is their check
        return state(it) != "new"
    lens = DATA["LENSES"][0] if it.get("trusted") else DATA["LENSES"][1]
    found = await run_agent("check", it,
        f"You are a checker for BitsOS V2 item {it['id']} (lane {it['lane']}) in {it['wt']} (branch lane/{it['lane']}). Do not edit or commit anything. Context:\n{DATA['COMMON']}"
        f"\n\nItem brief:\n{it['brief']}\n\nBuilder report:\n{built or '(see git log and the notes file of the item)'}\n\nYour lens: {lens['text']}"
        "\nAlso run the item's tests. Report only real, specific findings (severity, file, problem, concrete fix); say 'no findings' if there are none.",
        "opus" if lens["key"] == "hal" else "sonnet", "high" if lens["key"] == "hal" else "medium", read_only=True)
    if found is None:
        return False
    fixed = await run_agent("fix", it, head + f"\nYou are the fixer for item {it['id']}. Brief:\n{it['brief']}\n\nChecker findings (verify each first; fix the real ones, say why you reject others):\n{found}"
        "\n\nRun the tests and cargo xtask test afterwards, update the notes file with a fixer table (finding, verdict, what changed), commit on the lane branch. End with a short report.",
        "opus", "high")
    return fixed is not None


async def run_chain(chain, sem):
    for it in chain:
        for dep in NEEDS.get(it["id"], []):
            d = BY_ID.get(dep)
            if d and state(d) == "new":
                log(f"{it['id']}: waits for {dep}, which is not built; chain ends here")
                return
        if stop:
            return
        async with sem:
            if not await run_item(it):
                return


async def main():
    chains = {}
    for it in DATA["ITEMS"]:
        if not ARGS.only or it["id"] in ARGS.only:
            chains.setdefault(it["chain"], []).append(it)
    for c in chains.values():
        log("chain: " + ", ".join(f"{it['id']} ({state(it)})" for it in c))
    if ARGS.dry:
        return
    sem = asyncio.Semaphore(ARGS.parallel)
    # Chains whose first item is marked "last" (an integration) start only after all the others ended.
    first = [c for c in chains.values() if not c[0].get("last")]
    after = [c for c in chains.values() if c[0].get("last")]
    await asyncio.gather(*(run_chain(c, sem) for c in first))
    await asyncio.gather(*(run_chain(c, sem) for c in after))
    log(f"finished, spent {spent:.2f} $")


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--items", default=str(HERE / "items.json"))
    ap.add_argument("--budget", type=float, default=190.0)
    ap.add_argument("--parallel", type=int, default=2)
    ap.add_argument("--only", type=lambda s: s.split(","), default=None)
    ap.add_argument("--dry", action="store_true")
    ARGS = ap.parse_args()
    OUT.mkdir(exist_ok=True)
    DATA = json.loads(Path(ARGS.items).read_text())
    BY_ID = {it["id"]: it for it in DATA["ITEMS"]}
    if not ARGS.dry:
        if not KEY_FILE.exists():
            sys.exit(f"no API key: {KEY_FILE} is missing (see README.md)")
        KEY = KEY_FILE.read_text().strip()
    asyncio.run(main())
