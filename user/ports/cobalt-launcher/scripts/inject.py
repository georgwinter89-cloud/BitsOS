# usage: inject.py <js-file> <url> : one DevTools session stays open (scripts added on
# new documents live only as long as the session), then opens <url>.
import json, os, sys, time, urllib.request, websocket
port = os.environ.get("YT_PORT", "29222")
tabs = [t for t in json.load(urllib.request.urlopen(f"http://localhost:{port}/json")) if t.get("type") == "page"]
ws = websocket.create_connection(tabs[0]["webSocketDebuggerUrl"], suppress_origin=True)
n = 0
def call(m, p):
    global n
    n += 1
    ws.send(json.dumps({"id": n, "method": m, "params": p}))
    while True:
        r = json.loads(ws.recv())
        if r.get("id") == n:
            print(m, json.dumps(r)[:200], flush=True)
            return r
call("Page.enable", {})
call("Page.addScriptToEvaluateOnNewDocument", {"source": open(sys.argv[1]).read()})
call("Page.navigate", {"url": sys.argv[2]})
ws.settimeout(None)
while True:
    try:
        ws.recv()
    except Exception:
        time.sleep(1)
