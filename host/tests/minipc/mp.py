"""The Mini-PC scripts' way to MCP: the address and the key store as host/mcp/mcp-call.mjs takes them
(<host[:port]>, --tls DIR|auto, --token-file F, token also from BITSOS_TOKEN), over mutual TLS with the pinned key
(host/tests/mcp/mcp_tls.py). Without --tls plain HTTP: QEMU's kernel server, for host/tests/lo/20-desktop-stop.sh."""
import http.client
import json
import os
import sys
from types import SimpleNamespace

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "mcp"))
import mcp_tls  # noqa: E402

DEFAULT_PORT = 8765


def options(argv, positional=1, flags=(), values=()):
    """-> .hostport, .tls, .token_file, .token, .rest (the positional arguments, hostport first), .flags, .values"""
    o = SimpleNamespace(tls=None, token_file=None, rest=[], flags={f: False for f in flags}, values={v: None for v in values})
    it = iter(argv)
    for a in it:
        if a == "--tls":
            o.tls = next(it, None)
        elif a == "--token-file":
            o.token_file = next(it, None)
        elif a in o.flags:
            o.flags[a] = True
        elif a in o.values:
            o.values[a] = next(it, None)
        elif a.startswith("--"):
            raise SystemExit(f"unknown option {a}")
        else:
            o.rest.append(a)
    if len(o.rest) != positional:
        raise SystemExit(f"want {positional} argument(s) before the options, got {o.rest}: <host[:port]> ... [--tls DIR|auto] [--token-file F]")
    o.hostport = o.rest[0] if ":" in o.rest[0] else f"{o.rest[0]}:{DEFAULT_PORT}"
    o.token = os.environ.get("BITSOS_TOKEN")
    if o.token_file:
        with open(o.token_file) as f:
            o.token = f.read().strip()
    if not o.token:
        raise SystemExit("no token: BITSOS_TOKEN or --token-file")
    return o


def call(o, name, args=None, timeout=900):
    """One tool call -> (isError, text, images). Not reachable or a wrong pin ends the script: 'not asked'."""
    try:
        return mcp_tls.call_full(o.hostport, o.token, name, args or {}, timeout, o.tls, o.token_file)
    except (mcp_tls.PinError, OSError, ValueError, http.client.HTTPException, json.JSONDecodeError) as e:
        raise SystemExit(f"RED: MCP {name} at {o.hostport}: {e}") from e
