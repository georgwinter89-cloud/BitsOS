# usage: cdp.py <js-expression> : evaluates in the first page over DevTools (port $YT_PORT)
import json, os, sys, urllib.request, websocket
port = os.environ.get("YT_PORT", "29222")
tabs = [t for t in json.load(urllib.request.urlopen(f"http://localhost:{port}/json")) if t.get("type") == "page"]
ws = websocket.create_connection(tabs[0]["webSocketDebuggerUrl"], suppress_origin=True, timeout=30)
ws.send(json.dumps({"id": 1, "method": "Runtime.evaluate",
                    "params": {"expression": sys.argv[1], "returnByValue": True, "awaitPromise": True}}))
while True:
    r = json.loads(ws.recv())
    if r.get("id") == 1:
        print(json.dumps(r.get("result", r).get("result", r)))
        break
