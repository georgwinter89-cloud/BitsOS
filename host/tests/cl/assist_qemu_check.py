#!/usr/bin/env python3
"""CL-3a on V2 through MCP: `assist` (in the image, run as uid 1000 with MCP `run`) against the fake provider
on the host (assist_fake_api.py, reached from the guest as 10.0.2.2). A text answer from each provider kind, a
conversation in which the model changes a setting and one in which it takes the change back (checked through the
kernel's own settings tools), one in which it asks the machine for its version through the kernel's local tools
socket and gets it (CL-T), "Nur lokal" refusing a cloud provider without opening a connection, a rate limit
waited out, and the key in no output.

Usage: assist_qemu_check.py <mcp host port> <token> <fake port> <fake key> <fake log> <run dir> <qmp socket>
"""
import http.client
import json
import os
import re
import subprocess
import sys
import time

PORT, TOKEN, FAKE_PORT, KEY, FAKE_LOG, RUN_DIR, QMP = int(sys.argv[1]), sys.argv[2], int(sys.argv[3]), sys.argv[4], sys.argv[5], sys.argv[6], sys.argv[7]
HOST = f"10.0.2.2:{FAKE_PORT}"  # the host's loopback as QEMU's user network shows it to the guest
next_id = 0
printed = []  # everything any program printed in the guest


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}{': ' + str(detail)[-1500:] if detail else ''}")
    print(f"ok: {what}")


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    body = {"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}}
    c.request("POST", "/mcp", json.dumps(body), {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    if r.status != 200:
        raise ValueError(f"{method}: HTTP {r.status} {data[:200]}")
    return json.loads(data)


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=180)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "cl3a", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def call(c, name, args=None):
    j = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    if "error" in j:
        return True, f"rpc error {j['error']}"
    r = j["result"]
    return r["isError"], "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


def run(c, path, args, stdin=None):
    """A program in the guest as uid 1000: (exit code, stdout, stderr)."""
    a = {"path": path, "args": args, "timeout_ms": 120000}
    if stdin is not None:
        a["stdin"] = stdin
    err, text = call(c, "run", a)
    printed.append(text)
    m = re.match(r"pid \d+ ended: exit (\d+)\n--- stdout \(\d+ bytes\) ---\n(.*)\n?--- stderr \(\d+ bytes\) ---\n(.*)", text, re.S)
    if err or not m:
        raise SystemExit(f"RED: run {path} {args}: {text[:1500]}")
    return int(m.group(1)), m.group(2), m.group(3)


def setting(c, key, value):
    code, out, errt = run(c, "/bin/settings", ["--json", "--source", "cl3a-check", "set", key, value])
    need(code == 0 and '"ok":true' in out, f"settings set {key}", out + errt)


def assist(c, *args, stdin=None):
    return run(c, "/bin/assist", list(args), stdin)


def scale(c):
    """theme.scale as the kernel's own MCP tool sees it (not through assist)."""
    err, text = call(c, "settings_get", {"key": "theme.scale"})
    need(not err, "settings_get theme.scale through the kernel", text)
    return json.loads(text)["value"]


def fake_log():
    with open(FAKE_LOG) as f:
        return [json.loads(l) for l in f if l.strip()]


def connections():
    return sum(1 for e in fake_log() if e.get("connection"))


def provider(c, pid, kind, address, model, place, key):
    code, out, errt = assist(c, "--add", pid)
    need(code == 0 and f"ai.provider.{pid}.kind" in out, f"assist --add {pid}", out + errt)
    setting(c, f"ai.provider.{pid}.kind", kind)
    setting(c, f"ai.provider.{pid}.address", address)
    setting(c, f"ai.provider.{pid}.model", model)
    setting(c, f"ai.provider.{pid}.place", place)
    if key:
        code, out, errt = assist(c, "--key", pid, stdin=key + "\n")
        need(code == 0 and "gespeichert" in out, f"assist --key {pid}", out + errt)


ROOT_CALLS = []


def as_root(c, command):
    """Runs `command` in the root console shell (MCP's run is always uid 1000): the script goes to /tmp over
    MCP, its start is typed on the USB keyboard through QMP. Returns what it printed, its exit code last."""
    # A file of its own per call: one root wrote last time cannot be removed by uid 1000 (/tmp is sticky),
    # and its old `code=` line would be read as this call's.
    ROOT_CALLS.append(command)
    result = f"/tmp/clroot.{len(ROOT_CALLS)}.out"
    script = f"( {command} ) > {result} 2>&1\necho code=$? >> {result}\n"
    code, out, errt = run(c, "/bin/sh", ["-c", f"/bin/busybox rm -f {result}; /bin/busybox cat > /tmp/clroot"], stdin=script)
    need(code == 0, "the script for the root console is in /tmp", out + errt)
    end = time.time() + 120
    while "init: started the console shell" not in open(os.path.join(RUN_DIR, "serial.log"), errors="replace").read():
        need(time.time() < end, "the console shell started")
        time.sleep(1)
    here = os.path.dirname(os.path.abspath(__file__))
    typed = subprocess.run([sys.executable, "-I", os.path.join(here, "..", "mcp", "qmp_type.py"), QMP, "\\nsh /tmp/clroot\\n"], capture_output=True, text=True)
    need(typed.returncode == 0, "the command was typed at the console", typed.stdout + typed.stderr)
    end = time.time() + 60
    while True:
        code, out, errt = run(c, "/bin/busybox", ["cat", result])
        if "code=" in out:
            return out
        need(time.time() < end, "the root console ran the command", out + errt)
        time.sleep(1)


def georg_sets(c, key, value):
    """A change only Georg's own yes makes (it lets the assistant do more unasked, settingsd leash.rs): uid 1000
    is refused, root at the console (the scenario's stand-in for his hand) makes it."""
    code, out, errt = run(c, "/bin/settings", ["--json", "--source", "cl3a-check", "set", key, value])
    need(code == 1 and '"error":"not_confirmer"' in out, f"settings set {key} {value} as uid 1000 is refused", out + errt)
    out = as_root(c, f"/bin/settings --json --confirm set {key} {value}")
    need('"ok":true' in out and out.rstrip().endswith("code=0"), f"root at the console sets {key} {value}", out)


def providers_page(c):
    """CL-5s, what the settings page "KI" will do: a name on the list makes the provider's keys in settingsd
    (no assist --add), `assist --test` asks one provider once, a name taken off the list removes the provider
    and its key after a yes."""
    code, out, errt = run(c, "/bin/settings", ["--json", "get", "ai.providers"])
    need(code == 0, "settings get ai.providers", out + errt)
    ids = [i.strip() for i in json.loads(out)["value"].split(",") if i.strip()]
    need("a1" in ids and "g1" in ids, "the list names the providers of this run", ids)
    setting(c, "ai.providers", ", ".join(ids + ["neu"]))
    code, out, errt = run(c, "/bin/settings", ["--json", "get", "ai.provider.neu.place"])
    need(code == 0 and json.loads(out)["value"] == "unset", "a name on the list makes the provider's keys, without assist", out + errt)
    code, out, errt = run(c, "/bin/settings", ["--json", "raw", '{"op":"get","key":"ai.provider.neu.key","owner":"assist"}'])
    need(code == 1 and '"error":"not_owner"' in out, "... and its key is still only the assistant's to read", out + errt)

    before, conns = len(fake_log()), connections()
    code, out, errt = assist(c, "--test", "neu")
    need(code == 2 and out.count("\n") == 1 and "fehlt die Art" in out, "assist --test of a half set up provider: exit 2, one line, what is missing", out + errt)
    need(len(fake_log()) == before, "... and no connection was opened")
    code, out, errt = assist(c, "--test", "a1")
    need(code == 0 and out == "Der Anbieter „a1“ ist erreichbar, das Modell „claude-opus-5-5“ hat geantwortet.\n", "assist --test a1: exit 0 and one German line", out + errt)
    sent = [e for e in fake_log()[before:] if "kind" in e]
    need(connections() == conns + 1 and len(sent) == 1 and sent[0]["kind"] == "anthropic" and sent[0]["key_ok"] and sent[0]["tools_offered"] == [] and sent[0]["machine_tools"] == [],
         "... from ONE connection and ONE request, with the key and without tools", sent)

    rest = [i for i in ids if i != "g1"]
    # EINS K8: the chat's fallback is the one list of providers left among the roles.
    setting(c, "ai.role.chat_fallback", "g1, a1")
    # INT-CL-4: the yes for it is only Georg's own. A command line of his user cannot give it, with --confirm or
    # without, and is told where he gives it; the same for clearing or replacing the stored key.
    tries = [["set", "ai.providers", ", ".join(rest)], ["--confirm", "set", "ai.providers", ", ".join(rest)],
             ["--confirm", "reset", "ai.provider.g1.key"], ["--confirm", "set", "ai.provider.g1.key", "x"]]
    for args in tries:
        code, out, errt = run(c, "/bin/settings", ["--json", *args])
        need(code == 1 and '"error":"not_confirmer"' in out and "g1" in out and "Seite „KI“" in out and "als root an der Konsole" in out,
             f"settings {' '.join(args[:3])} as uid 1000 is refused and told where Georg confirms", out + errt)
    code, out, errt = run(c, "/bin/settings", ["--json", "get", "ai.providers"])
    need(code == 0 and [i.strip() for i in json.loads(out)["value"].split(",")] == ids + ["neu"], "... the list is as it was", out + errt)
    code, out, errt = run(c, "/bin/settings", ["--json", "get", "ai.provider.g1.kind"])
    need(code == 0 and json.loads(out)["value"] == "gemini", "... the provider too", out + errt)
    before = len(fake_log())
    code, out, errt = assist(c, "--test", "g1")
    sent = [e for e in fake_log()[before:] if "kind" in e]
    need(code == 0 and len(sent) == 1 and sent[0]["kind"] == "gemini" and sent[0]["key_ok"], "... and its key is still the stored one", out + errt + str(sent))
    # Root at the console is Georg: its --confirm removes the provider.
    out = as_root(c, f'/bin/settings --json status; /bin/settings --json --confirm set ai.providers "{", ".join(rest)}"')
    need('"uid":0' in out and out.count('"ok":true') == 2 and out.rstrip().endswith("code=0"), "root at the console removes the provider with --confirm", out)
    for pid in ("g1", "neu"):
        code, out, errt = run(c, "/bin/settings", ["--json", "get", f"ai.provider.{pid}.kind"])
        need(code == 1 and '"error":"unknown_key"' in out, f"the keys of {pid} are gone", out + errt)
    code, out, errt = run(c, "/bin/settings", ["--json", "get", "ai.role.chat_fallback"])
    need(code == 0 and json.loads(out)["value"] == "a1", "... and no role names the removed provider any more", out + errt)
    setting(c, "ai.role.chat_fallback", "")
    code, out, errt = assist(c, "--test", "g1")
    need(code == 2 and "nicht eingerichtet" in out, "assist --test of the removed provider: exit 2", out + errt)
    code, out, errt = assist(c, "--test", "a1")
    need(code == 0, "the other providers still answer", out + errt)


def helper_with_tools(c):
    """CL-6: a helper of version 2. It asks at the assistant's bell before its "tool" runs a command, tells what
    the tool did, and hands BitsOS's own tools on (`assist --mcp --bell`). `--once` has nobody to ask: what needs
    Georg's yes is refused with that reason and nothing runs. EINS K2: the one level `ai.level` decides for the
    helper's own tools too, and only Georg loosens it: uid 1000 with --confirm is refused, root at the console
    (the scenario's stand-in for his own hand) sets Automodus, and the same question is then a yes without anybody
    being asked; at Nur antworten the helper gets no bell at all."""
    rpc_call = lambda i, name, args: json.dumps({"jsonrpc": "2.0", "id": i, "method": "tools/call", "params": {"name": name, "arguments": args}})
    helper = ("#!/bin/sh\nread -r line\necho \"$line\" > /tmp/cl-helper2.request\n"
              "bell=$(echo \"$line\" | /bin/busybox sed -n 's/.*\"bell\":\"\\([^\"]*\\)\".*/\\1/p')\n"
              "echo '{\"ask\":\"Bash\",\"id\":\"q1\",\"kind\":\"run\",\"text\":\"touch /tmp/cl-helper2.made\"}' | /bin/assist --ask --bell \"$bell\" > /tmp/cl-helper2.reply\n"
              "case \"$(/bin/busybox cat /tmp/cl-helper2.reply)\" in *'\"answer\":\"yes\"'*) /bin/busybox touch /tmp/cl-helper2.made; echo '{\"tool\":\"Bash\",\"state\":\"done\",\"text\":\"\"}';; esac\n"
              f"printf '%s\\n' '{rpc_call(1, 'settings_get', {'key': 'theme.scale'})}' '{rpc_call(2, 'settings_set', {'key': 'theme.scale', 'value': 175})}' '{rpc_call(3, 'version', {})}'"
              " | /bin/assist --mcp --bell \"$bell\" > /tmp/cl-helper2.mcp 2> /tmp/cl-helper2.mcperr\n"
              "echo '{\"text\":\"Helfer mit Werkzeug fertig.\"}'\necho '{\"done\":true,\"stop\":\"end\",\"error\":null}'\n")
    err, text = call(c, "write_file", {"path": "/tmp/cl-helper2", "text": helper, "mode": "0755"})
    need(not err, "the helper script with a tool is in the guest", text)
    provider(c, "h2", "program", "/tmp/cl-helper2", "egal", "local", None)
    setting(c, "ai.role.chat", "h2")
    was = scale(c)
    show = "for f in reply mcp mcperr; do echo \"== $f\"; /bin/busybox cat /tmp/cl-helper2.$f; done; [ -e /tmp/cl-helper2.made ] && echo MADE || echo NOT-MADE"

    def round(what):
        run(c, "/bin/busybox", ["rm", "-f", "/tmp/cl-helper2.made"])
        code, out, errt = assist(c, "--once", "Leg die Datei an")
        need(code == 0 and out == "Helfer mit Werkzeug fertig.\n", f"{what}: the helper answers", out + errt)
        _, files, _ = run(c, "/bin/sh", ["-c", show])
        return errt, files

    errt, files = round("nobody to ask")
    need('"id":"q1","answer":"no","why":"cannot_ask"' in files and "NOT-MADE" in files, "the helper's question went through the bell; --once has nobody to ask, so the answer is no", files)
    need("Nicht ausgeführt, weil niemand gefragt werden kann: Befehl ausführen: touch /tmp/cl-helper2.made" in errt, "... and the screen says what was not run", errt)
    mcp = [json.loads(l) for l in files.split("== mcp\n")[1].split("== mcperr")[0].splitlines() if l.strip()]
    texts = {m["id"]: (m["result"]["content"][0]["text"], m["result"]["isError"]) for m in mcp}
    need(len(mcp) == 3 and not texts[1][1] and str(was) in texts[1][0], "assist --mcp under the helper: a reading tool runs", files)
    need(texts[2][1] and "kann ihn niemand fragen" in texts[2][0] and scale(c) == was, "... a changing tool is put to the assistant above (which cannot ask here) and changes nothing", files)
    need(not texts[3][1] and "BitsOS" in texts[3][0], "... and the machine's own tools are there (the kernel's door took this server)", files)
    need("not usable" not in files and "Einstellung „theme.scale“ ändern" in errt, "... it rang the bell of the assistant it descends from (the kernel named it), and the question reached that screen", files + errt)

    # Only Georg loosens the level. A command line of his user cannot, confirmed or not.
    for args in (["set", "ai.level", "auto"], ["--confirm", "set", "ai.level", "auto"]):
        code, out, errt = run(c, "/bin/settings", ["--json", *args])
        need(code == 1 and '"error":"not_confirmer"' in out and "als root an der Konsole" in out, f"settings {' '.join(args[:2])} ai.level auto as uid 1000 is refused", out + errt)
    out = as_root(c, "/bin/settings --json --confirm set ai.level auto")
    need('"ok":true' in out and out.rstrip().endswith("code=0"), "root at the console sets Automodus", out)
    errt, files = round("Automodus")
    need('"id":"q1","answer":"yes"' in files and "\nMADE" in files and "Befehl ausführen: touch /tmp/cl-helper2.made" in errt and "niemand gefragt werden kann: Befehl" not in errt,
         "in Automodus the same question is a yes at once, and the command ran", files + errt)
    need(scale(c) == 175, "... and BitsOS's own changing tool runs too: the one level counts for both", files)
    _, req, _ = run(c, "/bin/busybox", ["cat", "/tmp/cl-helper2.request"])
    need('"level":"auto"' in req and '"bitsos_helper":5' in req and '"run":"always"' in req, "... the helper is told the level and what runs unasked", req)
    setting(c, "theme.scale", str(was))
    # Tightening is anyone's: at Nur antworten the helper has no bell and is told its own tools are off.
    setting(c, "ai.level", "answer")
    errt, files = round("Nur antworten")
    _, req, _ = run(c, "/bin/busybox", ["cat", "/tmp/cl-helper2.request"])
    need("NOT-MADE" in files and '"own_tools":false' in req and '"bell"' not in req and '"level":"answer"' in req, "at Nur antworten no bell, no tools of its own, and nothing ran", files + req)
    # Back up to `ask` would be loosening again, which is root's at the console.
    code, out, errt = run(c, "/bin/settings", ["--json", "set", "ai.level", "ask"])
    need(code == 1 and '"error":"not_confirmer"' in out, "from Nur antworten back to Fragt vorher is loosening too, and refused for uid 1000", out + errt)
    out = as_root(c, "/bin/settings --json --confirm set ai.level ask")
    need('"ok":true' in out, "root sets Fragt vorher again", out)
    need(scale(c) == was, "the setting the helper's model wanted to change is as it was")
    setting(c, "ai.role.chat", "h1")


def main():
    c = connect(240)
    end = time.time() + 120
    while True:
        err, text = call(c, "run", {"path": "/bin/settings", "args": ["--json", "status"], "timeout_ms": 20000})
        if not err and '"ok":true' in text:
            break
        need(time.time() < end, "settingsd answers on its socket", text)
        time.sleep(1)
    need(scale(c) == 125, "theme.scale starts at its default 125")
    # DECISIONS 18.11.: the level is Automodus and `assist --mcp` may hand on the changing tools until Georg chooses
    # less. This scenario is about what asks: it tightens both first, which is anyone's.
    err, text = call(c, "settings_get", {"key": "ai.level"})
    need(not err and json.loads(text)["value"] == "auto", "ai.level exists and is Automodus (auto) until Georg chooses less", text)
    err, text = call(c, "settings_get", {"key": "ai.mcp.ask_tools"})
    need(not err and json.loads(text)["value"] is True, "ai.mcp.ask_tools is on until Georg turns it off", text)
    setting(c, "ai.level", "ask")
    setting(c, "ai.mcp.ask_tools", "false")

    # Nothing is configured in a fresh BitsOS: assist explains and contacts nobody.
    code, out, _ = assist(c, "--providers")
    need(code == 0 and "kein KI-Anbieter eingerichtet" in out and "assist --add NAME" in out, "assist --providers without providers explains how to add one", out)
    code, out, errt = assist(c, "--once", "Hallo")
    need(code == 1 and out == "" and "assist --add NAME" in errt, "assist --once without a provider does the same", errt)
    need(connections() == 0, "... and opened no connection")

    # A text answer from each kind. http is allowed: every provider is local and 10.0.2.2 is a private address.
    kinds = [("a1", "anthropic", f"http://{HOST}", "claude-opus-5-5", KEY, "ANTHROPIC"),
             ("o1", "openai", f"http://{HOST}/v1", "gpt-rec", KEY, "OPENAI"),
             ("g1", "gemini", f"http://{HOST}/v1beta", "gemini-rec", KEY, "GEMINI"),
             ("l1", "openai_compatible", f"http://{HOST}/v1", "local-llama", None, "OPENAI")]
    for pid, kind, address, model, key, mark in kinds:
        provider(c, pid, kind, address, model, "local", key)
        setting(c, "ai.role.chat", pid)
        code, out, errt = assist(c, "--once", "Hallo, grüß dich")
        want = f"Hallo Georg, hier antwortet die Aufzeichnung {mark}-OK. Grüße mit Umlauten: äöüß.\n"
        need(code == 0 and out == want, f"{kind}: assist --once prints the text answer", out + errt)
    reqs = [e for e in fake_log() if "kind" in e]
    need([e["kind"] for e in reqs] == ["anthropic", "openai", "gemini", "openai"] and all(e["key_ok"] for e in reqs), "the fake saw one request per kind, each with the right key (none for the local engine)", reqs)
    need(all(e["user"] == "Hallo, grüß dich" and len(e["tools_offered"]) == 6 for e in reqs), "... with the question and the six settings tools", reqs)
    # EINS K3: the native tools are every chat's, under the one level; the prompt names it.
    need(all(len(e["native_tools"]) == 10 and e["level_named"] == "Fragt vorher" for e in reqs), "... and the ten native tools, under the level Fragt vorher", [(e["native_tools"], e["level_named"]) for e in reqs])

    # CL-T: the machine's own tools come from the kernel's local tools socket. The door admitted /bin/assist
    # (no sentence about missing tools), every provider kind was offered them, settingsd's six only once.
    need(all(e["machine_tools"] == ["cpus", "metrics", "sched", "version"] and e["settings_set_named"] == 1 for e in reqs), "... and the kernel's local tools (cpus, metrics, sched, version), each tool once", reqs)
    setting(c, "ai.role.chat", "a1")
    code, out, errt = assist(c, "--once", "VERSION bitte")
    need(code == 0 and out == "Ich frage den Rechner.Erledigt: ANTHROPIC-DONE.\n", "the model asks for the machine's version and the conversation ends", out + errt)
    need("Werkzeug „version“ aufrufen" in errt and "nicht erreichbar" not in errt, "... the call ran without a question (a tool that only tells) and no tool is missing", errt)
    last = fake_log()[-1]
    need(len(last["tool_results"]) == 1 and last["tool_results"][0].startswith("BitsOS V2 kernel"), "... and the kernel's answer went back to the model", last)
    err, text = call(c, "settings_get", {"key": "ai.level"})
    need(not err and json.loads(text)["value"] == "ask", "ai.level is Fragt vorher (ask), as tightened at the start", text)
    err, text = call(c, "settings_get", {"key": "ai.tools.version.allow"})
    need(err or "unknown_key" in text, "the per-tool levels of before EINS are gone", text)
    # Nur antworten: the reading tools stay, nothing that changes is offered.
    setting(c, "ai.level", "answer")
    code, out, errt = assist(c, "--once", "VERSION bitte")
    first = [e for e in fake_log() if "kind" in e][-2]
    need(code == 0 and first["machine_tools"] == ["cpus", "metrics", "sched", "version"] and first["tools_offered"] == ["settings_describe", "settings_get", "settings_history", "settings_list"], "at Nur antworten the reading tools are offered and the changing ones are not", first)
    need(first["native_tools"] == ["befehl_ausgabe", "datei_lesen", "dateien_finden", "text_suchen"] and first["level_named"] == "Nur antworten", "... of the native tools only the reading ones, and the prompt says the level", first)
    georg_sets(c, "ai.level", "ask")
    # assist --mcp: the same tools for another program. A client that speaks the Model Context Protocol on the
    # pipes of `/bin/assist --mcp` (here: these lines) gets the list and the machine's version through the
    # kernel's local tools socket; a tool that changes something answers how to allow it and changes nothing.
    lines = [{"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "cl-mcp", "version": "1"}}},
             {"jsonrpc": "2.0", "method": "notifications/initialized"},
             {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
             {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "version", "arguments": {}}},
             {"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "settings_set", "arguments": {"key": "theme.scale", "value": 150}}}]
    script = "".join(json.dumps(l) + "\n" for l in lines)
    code, out, errt = assist(c, "--mcp", stdin=script)
    answers = [json.loads(l) for l in out.splitlines() if l.strip()]
    need(code == 0 and [a.get("id") for a in answers] == [1, 2, 3, 4] and answers[0]["result"]["protocolVersion"] == "2025-06-18", "assist --mcp answers a client's four requests on its pipes", out + errt)
    listed = {t["name"]: t["annotations"] for t in answers[1]["result"]["tools"]}
    need(sorted(listed) == sorted(["cpus", "metrics", "sched", "version", "settings_list", "settings_describe", "settings_get", "settings_set", "settings_undo", "settings_history"]), "... lists the kernel's local tools and settingsd's six", sorted(listed))
    need(listed["version"]["readOnlyHint"] is True and listed["settings_set"]["readOnlyHint"] is False, "... and says which of them change something", listed)
    told = answers[2]["result"]
    need(told["isError"] is False and told["content"][0]["text"].startswith("BitsOS V2 kernel") and "nicht erreichbar" not in errt, "... and the machine's version comes through the kernel's local tools socket", str(told) + errt)
    told = answers[3]["result"]
    need(told["isError"] is True and "settings --confirm set ai.mcp.ask_tools true" in told["content"][0]["text"] and scale(c) == 125, "... a change is refused with the way to allow it, and nothing changed", told)
    code, out, errt = assist(c, "--mcp", "--read-only", stdin=script)
    answers = [json.loads(l) for l in out.splitlines() if l.strip()]
    names = [t["name"] for t in answers[1]["result"]["tools"]] if code == 0 and len(answers) == 4 else []
    need("version" in names and "settings_set" not in names and "settings_undo" not in names and "error" in answers[3], "assist --mcp --read-only offers only what reads", out + errt)

    # An assistant whose settings socket was named from outside (here: the real one, by its path) does not go
    # to the kernel's door: it says in one sentence that only the settings tools are there, and goes on.
    code, out, errt = assist(c, "--socket", "/run/bitsos/settings.sock", "--once", "VERSION bitte")
    first, last = [e for e in fake_log() if "kind" in e][-2:]
    need(code == 0 and "Die Werkzeuge für diesen Rechner sind gerade nicht erreichbar; ich kann nur die Einstellungen lesen und ändern." in errt, "steered from outside, the assistant says that only the settings tools are there", out + errt)
    need(first["machine_tools"] == [] and len(first["tools_offered"]) == 6 and any("unknown tool" in t for t in last["tool_results"]) and out.endswith("ANTHROPIC-DONE.\n"), "... offers the six settings tools alone, and the conversation still ends", [first, last, out])

    # At Fragt vorher a change needs a yes; --once has nobody to ask, so nothing changes and it says so.
    setting(c, "ai.role.chat", "a1")
    code, out, errt = assist(c, "--once", "TOOL bitte")
    need(code == 0 and "Nicht ausgeführt, weil niemand gefragt werden kann" in errt and "theme.scale" in errt, "settings_set asks at Fragt vorher; --once refuses and says so", errt)
    need(scale(c) == 125, "... and theme.scale is unchanged")
    need(any("nobody can be asked" in t for t in fake_log()[-1]["tool_results"]), "... and the model was told", fake_log()[-1])

    # Automodus, by Georg's own hand: the model changes the setting, and it really changes.
    georg_sets(c, "ai.level", "auto")
    code, out, errt = assist(c, "--once", "TOOL bitte")
    need(code == 0 and out.endswith("Erledigt: ANTHROPIC-DONE.\n"), "the tool conversation ends with the model's closing text", out + errt)
    need(scale(c) == 150, "theme.scale is 150: the model's settings_set went through")
    last = fake_log()[-1]
    need(any('"new":150' in t for t in last["tool_results"]) and any('"value":125' in t for t in last["tool_results"]), "both tool results went back to the model", last)
    code, out, errt = assist(c, "--once", "TOOL-UNDO")
    need(code == 0 and "ANTHROPIC-DONE" in out, "the undo conversation ends", out + errt)
    need(scale(c) == 125, "theme.scale is 125 again: settings_undo brought it back")
    err, text = call(c, "settings_history", {"n": 2, "prefix": "theme"})
    need(not err and text.count('"source":"assist"') == 2, "the journal names assist as the source of both changes", text)
    setting(c, "ai.level", "ask")

    # A rate limit with Retry-After is waited out.
    code, out, errt = assist(c, "--once", "RATE bitte")
    need(code == 0 and "ANTHROPIC-OK" in out and "bittet um eine Pause" in errt, "a 429 with Retry-After is waited out and tried again", out + errt)

    # "Nur lokal": the cloud provider is refused before any connection is opened.
    provider(c, "wolke", "anthropic", f"https://{HOST}", "claude-opus-5-5", "cloud", KEY)
    setting(c, "ai.role.chat", "wolke")
    setting(c, "ai.local_only", "true")
    before = connections()
    code, out, errt = assist(c, "--once", "Hallo")
    need(code == 1 and out == "" and "Nur lokal" in errt and "wolke" in errt, "Nur lokal refuses the cloud provider with a German sentence", errt)
    need(connections() == before, "... and no connection was opened")
    setting(c, "ai.role.chat_fallback", "l1")
    code, out, errt = assist(c, "--once", "Hallo")
    need(code == 0 and "OPENAI-OK" in out and "Ich nehme „l1“" in errt, "with a local fallback the chat goes there", out + errt)
    need(connections() == before + 1 and fake_log()[-1]["model"] == "local-llama", "... one connection, to the local engine")
    setting(c, "ai.local_only", "false")

    code, out, _ = assist(c, "--providers")
    need(code == 0 and all(f"  {p}: " in out for p in ("a1", "o1", "g1", "l1", "wolke")), "assist --providers lists the five providers", out)
    need(out.count("Schlüssel gesetzt") == 4 and "l1: openai_compatible · lokal" in out and "wolke: anthropic · Cloud" in out, "... with kind, place and whether a key is set", out)

    # CL-S: settingsd takes who asks from the kernel (SO_PEERCRED, /proc/<pid>/exe), not from the request.
    # Another program that names the owner gets no key and may not replace it on its own word.
    ask = '{"op":"get","key":"ai.provider.a1.key","owner":"assist"}'
    code, out, errt = run(c, "/bin/settings", ["--json", "raw", ask])
    need(code == 1 and '"error":"not_owner"' in out and KEY not in out + errt, "another program that names the owner is refused the key", out + errt)
    code, out, errt = run(c, "/bin/settings", ["--json", "raw", '{"op":"set","key":"ai.provider.a1.key","value":"x"}'])
    need(code == 1 and '"error":"not_owner"' in out, "... and may not overwrite it without Georg's yes", out + errt)
    setting(c, "ai.role.chat", "a1")
    code, out, errt = assist(c, "--once", "Hallo")
    need(code == 0 and "ANTHROPIC-OK" in out and fake_log()[-1]["key_ok"], "the owner still reads its key afterwards", out + errt)

    # INT-CL3T: who asks is the kernel's record of the program that called connect (SO_PEERSEC), not a look
    # at /proc after the fact; `status` tells the asker what settingsd takes it for.
    code, out, errt = run(c, "/bin/settings", ["--json", "status"])
    asker = json.loads(out).get("asker", {}) if code == 0 else {}
    need(asker.get("by") == "kernel" and asker.get("exe") == "/bin/settings" and asker.get("uid") == 1000, "settingsd tells the asking program by the kernel's record of connect", out + errt)

    # A stored key goes only where it was entered for: nobody moves the provider under it, confirmed or not.
    for extra in ([], ["--confirm"]):
        for field, value in (("address", "http://10.0.2.2:1/evil"), ("kind", "openai"), ("place", "cloud")):
            code, out, errt = run(c, "/bin/settings", ["--json", *extra, "set", f"ai.provider.a1.{field}", value])
            need(code == 1 and '"error":"key_bound"' in out and KEY not in out + errt, f"settings set ai.provider.a1.{field} {' '.join(extra)} is refused while the key is stored", out + errt)
    code, out, errt = run(c, "/bin/settings", ["--json", "set", "ai.provider.a1.model", "claude-rec-2"])
    need(code == 0 and '"ok":true' in out, "... the model, which moves no key, still changes", out + errt)
    setting(c, "ai.provider.a1.model", "claude-opus-5-5")
    code, out, errt = assist(c, "--key", "a1", stdin=KEY + "\n")
    need(code == 0 and "gespeichert" in out and f"wird nur hierhin geschickt: http://{HOST}" in errt, "assist --key says where the key will go before it takes it", out + errt)
    code, out, errt = assist(c, "--once", "Hallo")
    need(code == 0 and "ANTHROPIC-OK" in out and fake_log()[-1]["key_ok"], "... and the provider still answers at its own address", out + errt)

    # The assistant is closed to the other programs of its user from its first instruction (the kernel does
    # that at execve, here through sh): its memory and what it runs are not theirs to open, another program's
    # are. `assist --key` never closes itself, so this is the kernel's doing.
    script = (
        "/bin/busybox sleep 8 2>/dev/null | /bin/assist --key a1 >/dev/null 2>&1 & A=$!; "
        "/bin/busybox sleep 8 >/dev/null 2>&1 & S=$!; /bin/busybox sleep 2; "
        "( : < /proc/$S/mem ) 2>/dev/null && echo other-open || echo other-closed; "
        "( : < /proc/$A/mem ) 2>/dev/null && echo assist-open || echo assist-closed; "
        "/bin/busybox readlink /proc/$S/exe >/dev/null 2>&1 && echo other-exe-shown || echo other-exe-hidden; "
        "/bin/busybox readlink /proc/$A/exe >/dev/null 2>&1 && echo assist-exe-shown || echo assist-exe-hidden; "
        "[ -d /proc/$A ] && echo assist-alive; kill $A $S 2>/dev/null; wait"
    )
    code, out, errt = run(c, "/bin/sh", ["-c", script])
    need(out.split() == ["other-open", "assist-closed", "other-exe-shown", "assist-exe-hidden", "assist-alive"], "another program of uid 1000 cannot open the running assistant's memory (it can open another program's)", out + errt)

    # The assistant inside a user and mount namespace somebody made (bwrap, as any program of uid 1000 may):
    # whoever made it reads and writes it and chose its files. settingsd gives it no key, the tools socket no
    # tools; the same program started plainly got both above.
    code, out, errt = run(c, "/bin/bwrap", ["--unshare-user", "--dev-bind", "/", "/", "/bin/assist", "--once", "Hallo"])
    need(code == 1 and "ließ sich nicht lesen (peer_unknown)" in errt and KEY not in out + errt, "an assistant in a namespace of its own gets no key from settingsd", f"exit {code}\n{out}\n{errt}")
    need("Die Werkzeuge für diesen Rechner sind gerade nicht erreichbar" in errt, "... and no tools from the local tools socket", errt)

    # A stranger at the tools socket, with a program every image has (the lean one has no mcp-fakesvc): the
    # `settings` command sends its line to any socket. The door answers with its refusal, or has hung up before
    # the line was written; a list of tools never comes.
    ask = '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
    code, out, errt = run(c, "/bin/settings", ["--json", "--socket", "/run/bitsos/tools.sock", "raw", ask])
    refused = code == 1 and "Refused: " in out and "does not run the boot's /bin/assist" in out
    hung_up = code == 2 and out == "" and ("Broken pipe" in errt or "closed the connection" in errt or "reset" in errt)
    need((refused or hung_up) and '"tools"' not in out, "another program at the local tools socket is turned away", f"exit {code}\n{out}\n{errt}")

    # Kind program: a helper program as the provider. A helper (a script put into the guest here) gets the conversation
    # as one line and answers in pieces; no provider is contacted for it. One that hangs is ended at
    # ai.timeout_s with everything it started, and the fallback answers.
    helper = ("#!/bin/sh\nread -r line\necho \"$line\" > /tmp/cl-helper.request\necho $$ > /tmp/cl-helper.pid\n"
              "case \"$line\" in *HAENGT*) /bin/busybox sleep 300;; esac\n"
              "echo '{\"text\":\"Hallo vom \"}'\necho '{\"note\":\"fast fertig\"}'\necho '{\"text\":\"Hilfsprogramm.\"}'\n"
              "echo '{\"done\":true,\"stop\":\"end\",\"error\":null}'\n")
    err, text = call(c, "write_file", {"path": "/tmp/cl-helper", "text": helper, "mode": "0755"})
    need(not err, "the helper script is in the guest", text)
    provider(c, "h1", "program", "/tmp/cl-helper", "egal", "local", None)
    setting(c, "ai.role.chat", "h1")
    before = connections()
    code, out, errt = assist(c, "--once", "Hallo Helfer")
    need(code == 0 and out == "Hallo vom Hilfsprogramm.\n" and "fast fertig" in errt, "kind program: the helper's pieces are the answer", out + errt)
    code, out, errt = run(c, "/bin/busybox", ["cat", "/tmp/cl-helper.request"])
    asked = json.loads(out) if code == 0 else {}
    need(asked.get("bitsos_helper") == 5 and asked.get("level") == "ask" and asked.get("messages") == [{"role": "user", "text": "Hallo Helfer"}] and connections() == before, "... it got the conversation as one line, and nobody else was asked", out + errt)
    setting(c, "ai.timeout_s", "3")
    code, out, errt = assist(c, "--once", "HAENGT bitte")
    need(code == 0 and "nicht rechtzeitig geantwortet" in errt and "Ich nehme „l1“" in errt and "OPENAI-OK" in out, "a helper that hangs is given up at ai.timeout_s and the fallback answers", out + errt)
    code, out, errt = run(c, "/bin/sh", ["-c", "p=$(/bin/busybox cat /tmp/cl-helper.pid); /bin/busybox sleep 1; [ -d /proc/$p ] && echo alive-$p || echo gone; /bin/busybox ps 2>/dev/null | /bin/busybox grep -c '[s]leep 300'"])
    need(out.split() == ["gone", "0"], "... and neither the helper nor what it started is left", out + errt)
    setting(c, "ai.timeout_s", "600")
    code, out, _ = assist(c, "--providers")
    need(code == 0 and "h1: program · lokal · /tmp/cl-helper" in out, "assist --providers shows the helper", out)
    helper_with_tools(c)

    # CL-3s: `assist serve`, the assistant as a service for the sidebar. It makes its socket for the user
    # alone and serves only the session's compositor, judged from the kernel's record of who connected.
    # This image runs no compositor, so every program here is a stranger: each of its lines (the `settings`
    # command sends one to any socket) gets not_allowed, also the yes to a tool question, nobody is asked
    # for it, and the service writes down whom it turned away. (The whole conversation over this socket is
    # in the host tests, tests/serve.rs, and in the sidebar's own scenario.) A second start gives up.
    setting(c, "ai.role.chat", "a1")
    sock = "/run/user/1000/bitsos-assist.sock"
    start = (f"/bin/assist serve </dev/null >/tmp/cl-serve.out 2>/tmp/cl-serve.err & echo $! > /tmp/cl-serve.pid; "
             f"i=0; while [ ! -S {sock} ] && [ $i -lt 100 ]; do /bin/busybox sleep 0.1; i=$((i+1)); done; "
             f"/bin/busybox stat -c '%a %u' {sock}")
    code, out, errt = run(c, "/bin/sh", ["-c", start])
    need(out.split() == ["600", "1000"], "assist serve makes its socket in the user's runtime folder, for the user alone", out + errt)
    before = connections()
    for line in ('{"op":"status"}', '{"op":"ask","id":1,"text":"Hallo"}', '{"op":"answer","ask_id":1,"yes":true}', '{"op":"history"}', '{"op":"new"}'):
        code, out, errt = run(c, "/bin/settings", ["--json", "--socket", sock, "raw", line])
        need(code == 1 and '"error":"not_allowed"' in out and "Oberfläche" in out and '"turns"' not in out and '"name"' not in out, f"a program that is not the compositor gets not_allowed for {line[:24]}", out + errt)
    need(connections() == before, "... and no provider was asked for it")
    code, out, errt = assist(c, "serve")
    need(code == 1 and "dort antwortet schon ein Assistent" in errt, "a second assist serve gives up with exit code 1", out + errt)
    # The sidebar takes whoever listens as the assistant when the kernel names /bin/assist. So on BitsOS the
    # real program does not serve with settings named from outside (they could name anybody as the provider).
    named = ("if [ -e /proc/bitsos/compositor ]; then /bin/assist --socket /tmp/cl-own-settings.sock serve --listen /tmp/cl-own.sock </dev/null; echo code=$?; "
             "SETTINGS_SOCKET=/tmp/cl-own-settings.sock /bin/assist serve --listen /tmp/cl-own.sock </dev/null; echo code=$?; "
             "SSL_CERT_FILE=/tmp/cl-own.pem /bin/assist serve --listen /tmp/cl-own.sock </dev/null; echo code=$?; else echo no holder file; fi; "
             "[ -e /tmp/cl-own.sock ] && echo socket made")
    code, out, errt = run(c, "/bin/sh", ["-c", named])
    need(out.split() == ["code=2"] * 3 and errt.count("nur mit den Einstellungen, Zertifikaten und Werkzeugen des Systems") == 3, "assist serve does not start with settings or certificates named from outside", out + errt)
    stop = ("p=$(/bin/busybox cat /tmp/cl-serve.pid); kill $p; i=0; while [ -d /proc/$p ] && [ $i -lt 50 ]; do /bin/busybox sleep 0.1; i=$((i+1)); done; "
            "[ -d /proc/$p ] && echo alive || echo gone; /bin/busybox cat /tmp/cl-serve.err")
    code, out, errt = run(c, "/bin/sh", ["-c", stop])
    need(out.startswith("gone") and "assist serve.start socket=" + sock + " uid=1000" in out, "the service says in its events that it started, and ends when told", out + errt)
    refused = [l for l in out.splitlines() if "assist serve.refused" in l]
    need(len(refused) == 5 and all("uid=1000 exe=/bin/settings by=kernel" in l for l in refused), "... and whom it turned away, by the kernel's record of who connected (one line per connection)", out)

    providers_page(c)

    err, hist = call(c, "settings_history", {"n": 200})
    err2, listed = call(c, "settings_list", {"prefix": "ai"})
    need(not err and not err2 and "***" in listed, "the keys show as *** in the settings list", listed[:300])
    everything = "\n".join(printed) + hist + listed + open(FAKE_LOG).read()
    need(KEY not in everything, "the key is in no program output, not in the history, the list or the fake's log")
    print("assist against the fake provider on V2: checks passed")


if __name__ == "__main__":
    main()
