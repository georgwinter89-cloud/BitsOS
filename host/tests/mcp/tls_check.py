"""MC-8b: what a host that is not paired gets from the MCP service after the pairing: nothing.

  tls_check.py plain  PORT TOKEN   a plain HTTP request with the right token: the service closes, no byte back
  tls_check.py nocert PORT TOKEN   TLS 1.3 without a client key, then the same request: no HTTP byte back

Exit 0 when nothing came back, 1 otherwise (the bytes are printed).
"""
import socket
import ssl
import sys


def request(token):
    body = b'{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"hw_info","arguments":{"section":"cpu"}}}'
    head = (
        "POST /mcp HTTP/1.1\r\nHost: bitsos\r\nAuthorization: Bearer %s\r\nContent-Type: application/json\r\n"
        "Content-Length: %d\r\n\r\n" % (token, len(body))
    )
    return head.encode() + body


def drain(s):
    got = b""
    try:
        while True:
            d = s.recv(4096)
            if not d:
                return got, "closed"
            got += d
    except (ssl.SSLError, OSError) as e:
        return got, "%s: %s" % (type(e).__name__, e)


def main():
    mode, port, token = sys.argv[1], int(sys.argv[2]), sys.argv[3]
    raw = socket.create_connection(("127.0.0.1", port), timeout=15)
    if mode == "plain":
        s = raw
        s.sendall(request(token))
    elif mode == "nocert":
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        ctx.minimum_version = ssl.TLSVersion.TLSv1_3
        try:
            s = ctx.wrap_socket(raw)
            s.sendall(request(token))
        except (ssl.SSLError, OSError) as e:
            print("%s: handshake ended: %s; nothing came back" % (mode, e))
            return 0
    else:
        print(__doc__)
        return 2
    got, how = drain(s)
    if got:
        print("%s: RED: %d bytes came back: %r" % (mode, len(got), got[:200]))
        return 1
    print("%s: nothing came back (%s)" % (mode, how))
    return 0


if __name__ == "__main__":
    sys.exit(main())
