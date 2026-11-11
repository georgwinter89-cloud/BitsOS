#!/usr/bin/env python3
"""MC-11: the desk's Python MCP calls over mutual TLS, as host/mcp/mcp-call.mjs makes them (--tls DIR|auto): this
host's key from the key store pair.mjs made (client.key, client.crt), the Mini-PC's certificate pinned
(pins/<host>.der); a connection that shows another key is refused before anything is sent. Under mcp=service
plain HTTP carries only the pairing, so every helper that calls the real Mini-PC (host/tests/nw/lan_rate.py)
goes through here.

As a module: store_of(tls, token_file), call(hostport, token, name, args, timeout, tls=None, token_file=None),
call_full (the same, with the answer's images).
As a command (for a quick look and the self-check in 05-desk-tools.sh):
  mcp_tls.py <host:port> <tool> [json] --tls DIR|auto [--token-file F]   (token: BITSOS_TOKEN or the file)
Exit 0 answer, 1 the tool's error, 2 not asked (pin, store, unreachable)."""
import base64
import http.client
import json
import os
import re
import ssl
import sys


class PinError(Exception):
    pass


def store_of(tls, token_file=None):
    """--tls's value: a directory, or auto (BITSOS_MCP_STORE, else bitsos-mcp/ next to the token file, else
    ~/.config/bitsos-mcp), as host/mcp/mcp-keys.mjs defaultStore."""
    if tls not in ("auto", True):
        return tls
    if os.environ.get("BITSOS_MCP_STORE"):
        return os.environ["BITSOS_MCP_STORE"]
    if token_file:
        return os.path.join(os.path.dirname(os.path.abspath(token_file)), "bitsos-mcp")
    return os.path.join(os.path.expanduser("~"), ".config", "bitsos-mcp")


def pin_file(store, host):
    name = re.sub(r"[^A-Za-z0-9.-]", "_", re.sub(r":\d+$", "", host))
    return os.path.join(store, "pins", f"{name}.der")


class Pinned(http.client.HTTPSConnection):
    """HTTPS with this host's client key; the server's certificate must be exactly the pinned one."""

    def __init__(self, host, port, store, timeout):
        crt, key = os.path.join(store, "client.crt"), os.path.join(store, "client.key")
        pf = pin_file(store, host)
        for f, what in ((crt, "client key"), (pf, f"pinned key for {host}")):
            if not os.path.exists(f):
                raise PinError(f"no {what} in {store}: pair first (node host/mcp/pair.mjs {host}:<port> --store {store})")
        with open(pf, "rb") as f:
            self.pin = f.read()
        self.pin_path = pf
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        ctx.minimum_version = ssl.TLSVersion.TLSv1_3
        # The pin is the check (self-signed keys, as mcp-call.mjs): no CA, no host name.
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        ctx.load_cert_chain(crt, key)
        super().__init__(host, port, timeout=timeout, context=ctx)

    def connect(self):
        super().connect()
        got = self.sock.getpeercert(binary_form=True)
        if got != self.pin:
            self.sock.close()
            raise PinError(f"REFUSED: the MCP service at {self.host}:{self.port} shows ANOTHER KEY than the paired one ({self.pin_path}); nothing was sent")


def call(hostport, token, name, args=None, timeout=330, tls=None, token_file=None):
    """One tools/call -> (isError, text). tls None: plain HTTP (QEMU's kernel server); else a store or auto."""
    err, text, _ = call_full(hostport, token, name, args, timeout, tls, token_file)
    return err, text


def call_full(hostport, token, name, args=None, timeout=330, tls=None, token_file=None):
    """`call`, with the answer's images: (isError, text, [png bytes])."""
    host, port = hostport.rsplit(":", 1)
    if tls:
        c = Pinned(host, int(port), store_of(tls, token_file), timeout)
    else:
        c = http.client.HTTPConnection(host, int(port), timeout=timeout)
    try:
        body = {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args or {}}}
        c.request("POST", "/mcp", json.dumps(body), {"Authorization": f"Bearer {token}", "Content-Type": "application/json", "Accept": "application/json, text/event-stream"})
        r = c.getresponse()
        data = r.read()
        if r.status == 403 and not tls:
            raise ValueError("HTTP 403: under mcp=service plain HTTP carries only the pairing; call with --tls DIR|auto")
        if r.status != 200:
            raise ValueError(f"HTTP {r.status}")
        j = json.loads(data)
        if "result" not in j:
            raise ValueError(f"{j}")
        text = "\n".join(i.get("text", "") for i in j["result"]["content"] if i["type"] == "text")
        images = [base64.b64decode(i["data"]) for i in j["result"]["content"] if i["type"] == "image"]
        return j["result"].get("isError", False), text, images
    finally:
        c.close()


def main(argv):
    opts, rest = {}, []
    it = iter(argv)
    for a in it:
        if a in ("--tls", "--token-file"):
            opts[a[2:]] = next(it, None)
        else:
            rest.append(a)
    if len(rest) not in (2, 3) or None in opts.values() or "tls" not in opts:
        print(__doc__.split("As a command")[1], file=sys.stderr)
        return 2
    token = os.environ.get("BITSOS_TOKEN")
    if opts.get("token-file"):
        with open(opts["token-file"]) as f:
            token = f.read().strip()
    if not token:
        print("no token: BITSOS_TOKEN or --token-file", file=sys.stderr)
        return 2
    args = json.loads(rest[2]) if len(rest) == 3 else {}
    try:
        err, text = call(rest[0], token, rest[1], args, 60, opts["tls"], opts.get("token-file"))
    except (PinError, OSError, ValueError, http.client.HTTPException) as e:
        print(e, file=sys.stderr)
        return 2
    print(text)
    return 1 if err else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
