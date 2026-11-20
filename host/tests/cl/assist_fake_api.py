#!/usr/bin/env python3
"""A fake AI provider for the assist scenarios (CL-3a). It speaks the three protocols assist has adapters for
and answers from the recordings in assist-rec/ (written by hand after the providers' documented stream formats;
no real provider is contacted, here or anywhere in the tests):

  POST /v1/messages                                   Anthropic Messages      (key in x-api-key)
  POST /v1/chat/completions                           OpenAI chat completions (key as bearer; none for model local-*)
  POST /v1beta/models/<model>:streamGenerateContent   Gemini                  (key in x-goog-api-key)

What it answers follows the newest user text: "TOOL-UNDO" a settings_undo call, "TOOL" a settings_get and a
settings_set call, "VERSION" a call of the machine's own tool `version` (CL-T; Anthropic only), "RATE" one 429
with Retry-After before the answer, anything else the text recording; once tool results came back, the closing
text. "LANGSAM" in the text makes the first answer wait a second (time to type
ahead of it). A wrong key gets the provider's 401. Every request is written as one JSON
line to the log (never the key itself, only whether it was right), which the checkers read.

Usage: assist_fake_api.py <port> <key> <log file> [bind address]
"""
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT, KEY, LOG = int(sys.argv[1]), sys.argv[2], sys.argv[3]
BIND = sys.argv[4] if len(sys.argv) > 4 else "127.0.0.1"
REC = os.path.join(os.path.dirname(os.path.abspath(__file__)), "assist-rec")
lock = threading.Lock()
state = {"n": 0, "rate_seen": set()}


def texts_anthropic(body):
    last = body["messages"][-1]
    content = last["content"] if isinstance(last["content"], list) else [{"type": "text", "text": last["content"]}]
    results = [b.get("content", "") for b in content if b.get("type") == "tool_result"]
    user = ""
    for m in body["messages"]:
        if m["role"] == "user":
            c = m["content"] if isinstance(m["content"], list) else [{"type": "text", "text": m["content"]}]
            t = "".join(b.get("text", "") for b in c if b.get("type") == "text")
            user = t or user
    return user, results


def texts_openai(body):
    msgs = body["messages"]
    results = []
    for m in reversed(msgs):
        if m["role"] != "tool":
            break
        results.insert(0, m.get("content", ""))
    user = next((m["content"] for m in reversed(msgs) if m["role"] == "user"), "")
    return user, results


def texts_gemini(body):
    contents = body["contents"]
    results = [json.dumps(p["functionResponse"]["response"]) for p in contents[-1]["parts"] if "functionResponse" in p]
    user = ""
    for c in contents:
        if c["role"] == "user":
            t = "".join(p.get("text", "") for p in c["parts"])
            user = t or user
    return user, results


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def reply(self, status, body, extra=()):
        data = body.encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        for k, v in extra:
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)

    def stream(self, name):
        with open(os.path.join(REC, name), "rb") as f:
            data = f.read()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        # Odd-sized pieces with a breath between them: events and characters get split as on a real line.
        for i in range(0, len(data), 53):
            piece = data[i:i + 53]
            self.wfile.write(b"%x\r\n%s\r\n" % (len(piece), piece))
            self.wfile.flush()
            time.sleep(0.002)
        self.wfile.write(b"0\r\n\r\n")

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        path = self.path
        try:
            body = json.loads(raw)
        except ValueError:
            return self.reply(400, '{"error":{"message":"not JSON"}}')
        if path == "/v1/messages":
            kind, key, model = "anthropic", self.headers.get("x-api-key", ""), body.get("model", "")
            user, results = texts_anthropic(body)
            ok = key == KEY and self.headers.get("anthropic-version") == "2026-10-21" and body.get("stream") is True and "max_tokens" in body
            denied = '{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"},"request_id":"req_fake"}'
            limited = '{"type":"error","error":{"type":"rate_limit_error","message":"This request would exceed your rate limit."}}'
        elif path == "/v1/chat/completions":
            kind, model = "openai", body.get("model", "")
            key = self.headers.get("Authorization", "")
            user, results = texts_openai(body)
            ok = (key == "" if model.startswith("local-") else key == f"Bearer {KEY}") and body.get("stream") is True
            denied = '{"error":{"message":"Incorrect API key provided.","type":"invalid_request_error","param":null,"code":"invalid_api_key"}}'
            limited = '{"error":{"message":"Rate limit reached.","type":"requests","param":null,"code":"rate_limit_exceeded"}}'
        elif path.startswith("/v1beta/models/") and ":streamGenerateContent" in path and path.endswith("?alt=sse"):
            kind, key = "gemini", self.headers.get("x-goog-api-key", "")
            model = path[len("/v1beta/models/"):].split(":")[0]
            user, results = texts_gemini(body)
            ok = key == KEY
            denied = '{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","status":"INVALID_ARGUMENT"}}'
            limited = '{"error":{"code":429,"message":"Resource has been exhausted.","status":"RESOURCE_EXHAUSTED"}}'
        else:
            return self.reply(404, '{"error":{"message":"no such endpoint"}}')
        tools = json.dumps(body.get("tools", []))
        entry = {"kind": kind, "path": path, "model": model, "key_ok": ok, "user": user, "tool_results": results,
                 "tools_offered": sorted(t for t in ("settings_list", "settings_describe", "settings_get", "settings_set", "settings_undo", "settings_history") if f'"{t}"' in tools),
                 # The kernel's own local tools (MC-L), as far as assist offered them.
                 "machine_tools": sorted(t for t in ("cpus", "metrics", "sched", "version") if f'"{t}"' in tools),
                 # How often each settings tool is named: once, although the kernel's list has them too.
                 "settings_set_named": tools.count('"settings_set"'),
                 # EINS K3: the native tools every chat has, as far as assist offered them (the level decides).
                 "native_tools": sorted(t for t in ("befehl", "befehl_ausgabe", "befehl_beenden", "datei_lesen", "datei_schreiben", "datei_aendern", "dateien_finden", "text_suchen", "web_holen", "loeschen") if f'"{t}"' in tools),
                 # The one system prompt names its level.
                 "level_named": next((l for l in ("Nur antworten", "Fragt vorher", "Automodus") if f"is \\\"{l}\\\"" in json.dumps(body)), None)}
        with lock:
            state["n"] += 1
            entry["n"] = state["n"]
            rate_first = "RATE" in user and (kind, user) not in state["rate_seen"]
            if rate_first:
                state["rate_seen"].add((kind, user))
        if not ok:
            entry["served"] = "401"
        elif rate_first:
            entry["served"] = "429"
        elif results:
            entry["served"] = f"{kind}-done.sse"
        elif "VERSION" in user and kind == "anthropic":
            entry["served"] = "anthropic-version.sse"
        elif "TOOL-UNDO" in user:
            entry["served"] = "anthropic-undo.sse" if kind == "anthropic" else f"{kind}-text.sse"
        elif "TOOL" in user:
            entry["served"] = f"{kind}-tools.sse"
        else:
            entry["served"] = f"{kind}-text.sse"
        with lock, open(LOG, "a") as f:
            f.write(json.dumps(entry, ensure_ascii=False) + "\n")
        if entry["served"] == "401":
            self.reply(400 if kind == "gemini" else 401, denied)
        elif entry["served"] == "429":
            self.reply(429, limited, [("Retry-After", "1")])
        else:
            if "LANGSAM" in user and not results:
                time.sleep(1.0)
            self.stream(entry["served"])
        self.close_connection = True


class Server(ThreadingHTTPServer):
    def get_request(self):
        # Every TCP connection counts, also one that never sends a request: "no connection was opened" is
        # something the checkers prove from this log.
        r = super().get_request()
        with lock, open(LOG, "a") as f:
            f.write(json.dumps({"connection": True}) + "\n")
        return r


if __name__ == "__main__":
    open(LOG, "a").close()
    srv = Server((BIND, PORT), Handler)
    srv.daemon_threads = True
    print(f"fake AI provider on {BIND}:{PORT}", flush=True)
    srv.serve_forever()
