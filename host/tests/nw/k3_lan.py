#!/usr/bin/env python3
"""Fake LAN for 60-sockets.sh (NW-5), the k3a/k3b-lan.mjs ideas of V1 for program sockets: QEMU's second card sends
its Ethernet frames as UDP datagrams (-netdev dgram) to this script, which is the router 192.168.77.1 (ARP, DHCP:
192.168.77.21 for an hour) and the host 192.168.77.50 (ARP answered 150 ms late, a real ARP wait), with TCP peers
written here frame by frame (the guest is always the client):

  7001  "hold":   SYN-ACK with a window of 1000; the first data segment is ignored (as if lost) until the guest
                  sends it again; then everything is taken; after the guest's FIN it answers "got <n>\\n" and FIN.
  7002  "silent": SYNs are dropped (a connect never completes).
  7003  "reset":  SYNs get RST (connection refused).
  7080  "lossy":  HTTP/1.1 GET /file (256 KiB, Content-Length), /chunked (the same, chunked), /file.sha256. The
                  answer's segments are lost (every 9th, first time only), duplicated (every 13th) and reordered
                  (every 11th goes after the next); lost ones come again on three duplicate ACKs or after 300 ms.

Every event goes to the log (one line each). Usage: k3_lan.py <listen_port> <qemu_port> <log>
"""
import hashlib
import random
import socket
import struct
import sys
import time

LISTEN, QEMU, LOG = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
ROUTER = bytes([192, 168, 77, 1])
FAR = bytes([192, 168, 77, 50])
GUEST = bytes([192, 168, 77, 21])
MAC = bytes.fromhex("02aabbccddee")
BCAST = b"\xff" * 6
T0 = time.time()
BODY = random.Random(5).randbytes(256 * 1024)
SHA = hashlib.sha256(BODY).hexdigest()
MSS = 1400


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
    c = csum(src + dst + struct.pack("!BBH", 0, 17, len(u)) + bytes(u)) or 0xFFFF
    u[6:8] = struct.pack("!H", c)
    return bytes(u)


def tcp(src, dst, sp, dp, seq, ack, flags, data=b"", win=65535, mss=False):
    opts = struct.pack("!BBH", 2, 4, 1460) if mss else b""
    off = (20 + len(opts)) // 4
    t = bytearray(struct.pack("!HHIIBBHHH", sp, dp, seq & 0xFFFFFFFF, ack & 0xFFFFFFFF, off << 4, flags, win, 0, 0) + opts + data)
    c = csum(src + dst + struct.pack("!BBH", 0, 6, len(t)) + bytes(t))
    t[16:18] = struct.pack("!H", c)
    return bytes(t)


FIN, SYN, RST, PSH, ACK = 1, 2, 4, 8, 16


class Conn:
    def __init__(self, lan, me, port, gport, gseq, gmac):
        self.lan, self.me, self.port, self.gport, self.gmac = lan, me, port, gport, gmac
        self.isn = random.randrange(1 << 32)
        self.rcv = (gseq + 1) & 0xFFFFFFFF
        self.una = self.nxt = (self.isn + 1) & 0xFFFFFFFF
        self.got = b""
        self.out = b""          # everything we will send after the SYN (sequence una0 + i)
        self.base = self.una    # the sequence number of out[0]
        self.sent_once = set()  # segment indices sent before (losses only the first time)
        self.held = None        # a reordered segment waiting for the next one
        self.dups = 0
        self.last_ack_time = time.time()
        self.fin_queued = False
        self.fin_sent = False
        self.guest_fin = False
        self.dropped_first = False
        self.win = 1000 if port == 7001 else 65535
        self.gwin = 65535
        self.closed = False

    def seg(self, seq, flags, data=b"", mss=False):
        self.lan.send(eth(self.gmac, 0x0800, ipv4(6, self.me, GUEST, tcp(self.me, GUEST, self.port, self.gport, seq, self.rcv, flags, data, self.win, mss))))

    def ack(self):
        self.seg(self.nxt, ACK)

    def on_data(self, seq, data, flags):
        if data:
            if self.port == 7001 and not self.dropped_first:
                self.dropped_first = True
                log(f"hold: first data segment ({len(data)} bytes) ignored")
                return
            if seq == self.rcv:
                self.got += data
                self.rcv = (self.rcv + len(data)) & 0xFFFFFFFF
                if self.port == 7001 and len(self.got) == len(data) and self.dropped_first:
                    log(f"hold: the resend came ({len(data)} bytes)")
            else:
                log(f"{self.port}: out-of-order data seq {seq - self.rcv:+d}")
        if flags & FIN and (seq + len(data)) & 0xFFFFFFFF == self.rcv and not self.guest_fin:
            self.guest_fin = True
            self.rcv = (self.rcv + 1) & 0xFFFFFFFF
            log(f"{self.port}: FIN from the guest after {len(self.got)} bytes")
            if self.port == 7001:
                self.queue(f"got {len(self.got)}\n".encode(), fin=True)
        self.ack()
        if self.port == 7080 and not self.out and b"\r\n\r\n" in self.got:
            self.http()

    def http(self):
        line = self.got.split(b"\r\n", 1)[0].decode(errors="replace")
        path = line.split(" ")[1] if " " in line else "/"
        log(f"lossy: {line}")
        if path == "/file":
            self.queue(b"HTTP/1.1 200 OK\r\nContent-Length: %d\r\nConnection: close\r\n\r\n" % len(BODY) + BODY, fin=True)
        elif path == "/chunked":
            parts, i, rng = [], 0, random.Random(9)
            while i < len(BODY):
                n = rng.choice([1, 100, 1400, 5000, 30000])
                parts.append(b"%x\r\n" % len(BODY[i:i + n]) + BODY[i:i + n] + b"\r\n")
                i += n
            self.queue(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n" + b"".join(parts) + b"0\r\n\r\n", fin=True)
        elif path == "/file.sha256":
            body = f"{SHA} {len(BODY)}\n".encode()
            self.queue(b"HTTP/1.1 200 OK\r\nContent-Length: %d\r\nConnection: close\r\n\r\n" % len(body) + body, fin=True)
        else:
            self.queue(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", fin=True)

    def queue(self, data, fin):
        self.out += data
        self.fin_queued = fin
        self.pump()

    def on_ack(self, ack, win):
        self.gwin = win
        off = (ack - self.una) & 0xFFFFFFFF
        if 0 < off <= (self.nxt - self.una) & 0xFFFFFFFF:
            self.una = ack
            self.dups = 0
            self.last_ack_time = time.time()
        elif ack == self.una and self.una != self.nxt:
            self.dups += 1
            if self.dups == 3:
                log(f"{self.port}: three duplicate ACKs, sending again from {(self.una - self.base) & 0xFFFFFFFF}")
                self.nxt = self.una
        self.pump()

    def pump(self):
        if self.closed:
            return
        end = len(self.out)
        while True:
            off = (self.nxt - self.base) & 0xFFFFFFFF
            inflight = (self.nxt - self.una) & 0xFFFFFFFF
            if off >= end or inflight >= self.gwin:
                break
            n = min(MSS, end - off, self.gwin - inflight)
            data = self.out[off:off + n]
            idx = off // MSS
            first = idx not in self.sent_once
            self.sent_once.add(idx)
            seq = self.nxt
            self.nxt = (self.nxt + n) & 0xFFFFFFFF
            if self.port == 7080 and first and idx % 9 == 4:
                log(f"lossy: segment {idx} lost")
                continue
            if self.port == 7080 and first and idx % 11 == 5 and self.held is None:
                self.held = (seq, data)
                continue
            self.seg(seq, ACK | PSH, data)
            if self.port == 7080 and first and idx % 13 == 0:
                self.seg(seq, ACK | PSH, data)
            if self.held is not None:
                log(f"lossy: segment after its successor")
                self.seg(self.held[0], ACK | PSH, self.held[1])
                self.held = None
        if self.held is not None and (self.nxt - self.base) & 0xFFFFFFFF >= end:
            self.seg(self.held[0], ACK | PSH, self.held[1])
            self.held = None
        if self.fin_queued and (self.nxt - self.base) & 0xFFFFFFFF == end and not self.fin_sent and self.una == self.nxt:
            self.fin_sent = True
            self.seg(self.nxt, FIN | ACK)
            self.nxt = (self.nxt + 1) & 0xFFFFFFFF
            log(f"{self.port}: FIN after {end} bytes")

    def tick(self):
        if self.una != self.nxt and time.time() - self.last_ack_time > 0.3:
            log(f"{self.port}: timeout, sending again from {(self.una - self.base) & 0xFFFFFFFF}")
            self.last_ack_time = time.time()
            if self.fin_sent and (self.una - self.base) & 0xFFFFFFFF >= len(self.out):
                self.seg((self.nxt - 1) & 0xFFFFFFFF, FIN | ACK)
                return
            self.fin_sent = False
            self.nxt = self.una
            self.pump()


class Lan:
    def __init__(self):
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.bind(("127.0.0.1", LISTEN))
        self.sock.settimeout(0.05)
        self.conns = {}
        self.later = []

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
        r += bytes([53, 1, reply, 54, 4]) + ROUTER + bytes([51, 4]) + struct.pack("!I", 3600) + bytes([1, 4, 255, 255, 255, 0, 3, 4]) + ROUTER + bytes([255])
        self.send(eth(BCAST, 0x0800, ipv4(17, ROUTER, b"\xff" * 4, udp(ROUTER, b"\xff" * 4, 67, 68, r))))
        log(f"dhcp: {'OFFER' if reply == 2 else 'ACK'} 192.168.77.21")

    def on_frame(self, f):
        if len(f) < 14:
            return
        typ = struct.unpack("!H", f[12:14])[0]
        if typ == 0x0806 and len(f) >= 42 and f[20:22] == b"\x00\x01" and f[38:42] in (ROUTER, FAR):
            who = f[38:42]
            reply = eth(f[6:12], 0x0806, f[14:20] + b"\x00\x02" + MAC + who + f[22:28] + f[28:32])
            if who == FAR:
                log("arp: 192.168.77.50 asked, answering in 150 ms")
                self.later.append((time.time() + 0.15, reply))
            else:
                self.send(reply)
            return
        if typ != 0x0800 or len(f) < 34:
            return
        h = f[14:]
        ihl = (h[0] & 15) * 4
        proto, src, dst = h[9], h[12:16], h[16:20]
        data = h[ihl:struct.unpack("!H", h[2:4])[0]]
        if proto == 17 and len(data) >= 8 and struct.unpack("!H", data[2:4])[0] == 67:
            self.dhcp(data[8:])
        elif proto == 6 and dst in (ROUTER, FAR) and len(data) >= 20:
            self.on_tcp(f[6:12], dst, data)

    def on_tcp(self, gmac, me, d):
        sp, dp, seq, ack, offf, flags, win = struct.unpack("!HHIIBBH", d[:16])
        data = d[(offf >> 4) * 4:]
        key = (me, dp, sp)
        if flags & SYN and not flags & ACK:
            if dp == 7002:
                log("silent: SYN dropped")
                return
            if dp == 7003 or dp not in (7001, 7080):
                log(f"reset: SYN to {dp} answered with RST")
                self.send(eth(gmac, 0x0800, ipv4(6, me, GUEST, tcp(me, GUEST, dp, sp, 0, seq + 1, RST | ACK, b"", 0))))
                return
            c = Conn(self, me, dp, sp, seq, gmac)
            self.conns[key] = c
            log(f"{dp}: SYN from port {sp} -> SYN-ACK window {c.win}")
            c.seg(c.isn, SYN | ACK, mss=True)
            return
        c = self.conns.get(key)
        if c is None:
            if not flags & RST:
                self.send(eth(gmac, 0x0800, ipv4(6, me, GUEST, tcp(me, GUEST, dp, sp, ack, seq + len(data), RST | ACK, b"", 0))))
            return
        if flags & RST:
            log(f"{dp}: RST from the guest")
            c.closed = True
            del self.conns[key]
            return
        if flags & ACK:
            c.on_ack(ack, win)
        if data or flags & FIN:
            c.on_data(seq, data, flags)
        if c.fin_sent and c.guest_fin and c.una == c.nxt:
            log(f"{dp}: closed both ways")
            del self.conns[key]

    def run(self):
        log(f"k3 LAN on 127.0.0.1:{LISTEN}, QEMU at {QEMU}; lossy body sha256 {SHA}")
        while True:
            try:
                f, _ = self.sock.recvfrom(65536)
                self.on_frame(f)
            except socket.timeout:
                pass
            now = time.time()
            for t, fr in [x for x in self.later if x[0] <= now]:
                self.send(fr)
            self.later = [x for x in self.later if x[0] > now]
            for c in list(self.conns.values()):
                c.tick()


Lan().run()
