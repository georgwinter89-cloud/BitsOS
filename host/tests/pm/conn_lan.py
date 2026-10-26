#!/usr/bin/env python3
"""Fake LAN for 10-connections.sh (PM-2): QEMU's second card sends its Ethernet frames as UDP datagrams
(-netdev dgram) to this script, which is the router 192.168.77.1 (ARP, DHCP: 192.168.77.21, DNS on port 53) and
the web hosts .50, .51, .52, .53 (port 80, TCP written here frame by frame; the guest is always the client).

DNS (192.168.77.1:53): www.google.com A .50; www.youtube.com CNAME youtube-ui.l.google.com, A .51;
search.brave.com A .52; everything else NXDOMAIN. Port 80: the guest sends one line, the host answers
"LEN <n>\\n" and n bytes (.50: 1000, .51: 40000, .52: 2500, .53: 700) and closes after the guest's FIN.
Every DNS answer and every connection's byte counts go to the log, one line each:
  dns <name> from <port> query=<bytes> answer=<bytes>
  tcp <ip>:80 port <guest port> in=<bytes from the guest> out=<bytes to the guest>
Usage: conn_lan.py <listen_port> <qemu_port> <log>
"""
import socket
import struct
import sys
import time

LISTEN, QEMU, LOG = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
NET = bytes([192, 168, 77])
ROUTER, GUEST = NET + b"\x01", NET + b"\x15"
MAC = bytes.fromhex("02aabbccdd77")
BCAST = b"\xff" * 6
SIZES = {50: 1000, 51: 40000, 52: 2500, 53: 700}
NAMES = {"www.google.com": [("A", 50)], "www.youtube.com": [("CNAME", "youtube-ui.l.google.com"), ("A", 51)],
         "search.brave.com": [("A", 52)]}
FIN, SYN, RST, PSH, ACK = 1, 2, 4, 8, 16
MSS = 1400
T0 = time.time()


def log(msg):
    with open(LOG, "a") as f:
        f.write(f"{time.time() - T0:8.3f} {msg}\n")


def csum(b):
    if len(b) % 2:
        b += b"\0"
    s = sum(struct.unpack(f"!{len(b) // 2}H", b))
    while s >> 16:
        s = (s & 0xFFFF) + (s >> 16)
    return ~s & 0xFFFF


def eth(dst, typ, payload):
    return dst + MAC + struct.pack("!H", typ) + payload


def ipv4(proto, src, dst, payload):
    h = bytearray(struct.pack("!BBHHHBBH4s4s", 0x45, 0, 20 + len(payload), 0x1234, 0, 64, proto, 0, src, dst))
    h[10:12] = struct.pack("!H", csum(bytes(h)))
    return bytes(h) + payload


def udp(src, dst, sp, dp, data):
    u = bytearray(struct.pack("!HHHH", sp, dp, 8 + len(data), 0) + data)
    u[6:8] = struct.pack("!H", csum(src + dst + struct.pack("!BBH", 0, 17, len(u)) + bytes(u)) or 0xFFFF)
    return bytes(u)


def tcp(src, dst, sp, dp, seq, ack, flags, data=b"", mss=False):
    opts = struct.pack("!BBH", 2, 4, MSS) if mss else b""
    t = bytearray(struct.pack("!HHIIBBHHH", sp, dp, seq & 0xFFFFFFFF, ack & 0xFFFFFFFF, ((20 + len(opts)) // 4) << 4, flags, 65535, 0, 0) + opts + data)
    t[16:18] = struct.pack("!H", csum(src + dst + struct.pack("!BBH", 0, 6, len(t)) + bytes(t)))
    return bytes(t)


def qname(b, i):
    labels = []
    while b[i]:
        labels.append(b[i + 1:i + 1 + b[i]].decode("ascii", "replace"))
        i += 1 + b[i]
    return ".".join(labels).lower(), i + 1


def enc(name):
    return b"".join(bytes([len(l)]) + l.encode() for l in name.split(".")) + b"\0"


def dns_answer(q):
    name, end = qname(q, 12)
    question = q[12:end + 4]
    rrs = NAMES.get(name)
    if rrs is None:
        return name, q[:2] + b"\x81\x83" + b"\x00\x01\x00\x00\x00\x00\x00\x00" + question
    body, owner = b"", b"\xc0\x0c"
    for kind, v in rrs:
        if kind == "CNAME":
            data = enc(v)
            body += owner + struct.pack("!HHIH", 5, 1, 300, len(data)) + data
            owner = enc(v)
        else:
            body += owner + struct.pack("!HHIH", 1, 1, 300, 4) + NET + bytes([v])
    return name, q[:2] + b"\x81\x80" + struct.pack("!HHHH", 1, len(rrs), 0, 0) + question + body


class Conn:
    def __init__(self, lan, me, gport, gseq, gmac):
        self.lan, self.me, self.gport, self.gmac = lan, me, gport, gmac
        self.rcv = gseq + 1
        self.isn = 1000
        self.una = self.nxt = self.isn + 1
        self.req, self.got = b"", 0
        self.out = b""
        self.sent_at = 0.0
        self.guest_fin = self.fin_sent = False
        self.win = 65535

    def seg(self, seq, flags, data=b"", mss=False):
        self.lan.send(eth(self.gmac, 0x0800, ipv4(6, self.me, GUEST, tcp(self.me, GUEST, 80, self.gport, seq, self.rcv, flags, data, mss))))

    def on_data(self, seq, data, flags):
        if seq != self.rcv:
            self.seg(self.nxt, ACK)
            return
        self.rcv += len(data)
        self.got += len(data)
        self.req += data
        if flags & FIN:
            self.rcv += 1
            self.guest_fin = True
        if b"\n" in self.req and not self.out:
            n = SIZES[self.me[3]]
            self.out = f"LEN {n}\n".encode() + b"x" * n
            self.pump()
        else:
            self.seg(self.nxt, ACK)
        if self.guest_fin and not self.fin_sent and self.una - self.isn - 1 == len(self.out):
            self.finish()

    def finish(self):
        self.fin_sent = True
        self.seg(self.nxt, FIN | ACK)
        self.nxt += 1
        log(f"tcp {socket.inet_ntoa(self.me)}:80 port {self.gport} in={self.got} out={len(self.out)}")

    def pump(self, again=False):
        if again:
            self.nxt = self.una
        base = self.isn + 1
        while self.nxt - base < len(self.out) and self.nxt - self.una < self.win:
            off = self.nxt - base
            chunk = self.out[off:off + min(MSS, self.win - (self.nxt - self.una))]
            if not chunk:
                break
            self.seg(self.nxt, ACK | PSH, chunk)
            self.nxt += len(chunk)
            self.sent_at = time.time()

    def on_ack(self, ack, win):
        self.win = max(win, 1)
        if self.una < ack <= self.nxt:
            self.una = ack
            self.pump()
        if self.guest_fin and not self.fin_sent and self.out and self.una - self.isn - 1 == len(self.out):
            self.finish()

    def tick(self):
        if self.out and self.una < self.nxt and time.time() - self.sent_at > 0.3:
            self.pump(again=True)


class Lan:
    def __init__(self):
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.bind(("127.0.0.1", LISTEN))
        self.sock.settimeout(0.05)
        self.conns = {}

    def send(self, frame):
        self.sock.sendto(frame, ("127.0.0.1", QEMU))

    def dhcp(self, b):
        if len(b) < 240 or b[0] != 1:
            return
        o, i = {}, 240
        while i < len(b) and b[i] != 255:
            if b[i] == 0:
                i += 1
                continue
            o[b[i]] = b[i + 2:i + 2 + b[i + 1]]
            i += 2 + b[i + 1]
        kind = o.get(53, b"\0")[0]
        if kind not in (1, 3):
            return
        reply = 2 if kind == 1 else 5
        xid, flags, ciaddr, chaddr = b[4:8], b[10:12], b[12:16], b[28:44]
        r = bytes([2, 1, 6, 0]) + xid + b"\0\0" + flags + ciaddr + GUEST + ROUTER + b"\0" * 4 + chaddr + b"\0" * 192 + bytes([0x63, 0x82, 0x53, 0x63])
        r += bytes([53, 1, reply, 54, 4]) + ROUTER + bytes([51, 4]) + struct.pack("!I", 3600) + bytes([1, 4, 255, 255, 255, 0, 3, 4]) + ROUTER
        r += bytes([6, 4]) + ROUTER + bytes([255])
        self.send(eth(BCAST, 0x0800, ipv4(17, ROUTER, b"\xff" * 4, udp(ROUTER, b"\xff" * 4, 67, 68, r))))
        log(f"dhcp: {'OFFER' if reply == 2 else 'ACK'} 192.168.77.21")

    def on_frame(self, f):
        if len(f) < 14:
            return
        typ = struct.unpack("!H", f[12:14])[0]
        if typ == 0x0806 and len(f) >= 42 and f[20:22] == b"\x00\x01" and f[38:41] == NET and f[41] in (1, *SIZES):
            self.send(eth(f[6:12], 0x0806, f[14:20] + b"\x00\x02" + MAC + f[38:42] + f[22:28] + f[28:32]))
            return
        if typ != 0x0800 or len(f) < 34:
            return
        h = f[14:]
        ihl = (h[0] & 15) * 4
        proto, dst = h[9], h[16:20]
        data = h[ihl:struct.unpack("!H", h[2:4])[0]]
        if proto == 17 and len(data) >= 8:
            sp, dp = struct.unpack("!HH", data[:4])
            if dp == 67:
                self.dhcp(data[8:])
            elif dp == 53 and dst == ROUTER and len(data) > 20:
                try:
                    name, a = dns_answer(data[8:])
                except (IndexError, UnicodeError):
                    return
                self.send(eth(f[6:12], 0x0800, ipv4(17, ROUTER, GUEST, udp(ROUTER, GUEST, 53, sp, a))))
                log(f"dns {name} from {sp} query={len(data) - 8} answer={len(a)}")
        elif proto == 6 and dst[:3] == NET and dst[3] in SIZES and len(data) >= 20:
            self.on_tcp(f[6:12], dst, data)

    def on_tcp(self, gmac, me, d):
        sp, dp, seq, ack, offf, flags, win = struct.unpack("!HHIIBBH", d[:16])
        data = d[(offf >> 4) * 4:]
        key = (me, sp)
        if flags & SYN and not flags & ACK:
            if dp != 80:
                self.send(eth(gmac, 0x0800, ipv4(6, me, GUEST, tcp(me, GUEST, dp, sp, 0, seq + 1, RST | ACK))))
                return
            c = Conn(self, me, sp, seq, gmac)
            self.conns[key] = c
            c.seg(c.isn, SYN | ACK, mss=True)
            return
        c = self.conns.get(key)
        if c is None:
            return
        if flags & RST:
            log(f"tcp {socket.inet_ntoa(me)}:80 port {sp} reset by the guest")
            del self.conns[key]
            return
        if flags & ACK:
            c.on_ack(ack, win)
        if data or flags & FIN:
            c.on_data(seq, data, flags)
        if c.fin_sent and c.una == c.nxt:
            del self.conns[key]

    def run(self):
        log(f"conn LAN on 127.0.0.1:{LISTEN}, QEMU at {QEMU}")
        while True:
            try:
                f, _ = self.sock.recvfrom(65536)
                self.on_frame(f)
            except socket.timeout:
                pass
            for c in list(self.conns.values()):
                c.tick()


Lan().run()
