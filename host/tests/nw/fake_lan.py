#!/usr/bin/env python3
"""Fake LAN for 40-fake-lan.sh (NW-3), the idea of V1's net-lan.mjs: QEMU's second card sends its Ethernet frames
as UDP datagrams (-netdev dgram) to this script, which plays the router 192.168.77.1:

- ARP: answers for 192.168.77.1.
- ICMP: answers echo requests to 192.168.77.1; once the guest is settled on .21 it pings the guest every second
  and logs "guest answered ping".
- DHCP, a scripted life: the first 3 DISCOVERs are ignored (repeated DISCOVER), then 192.168.77.20 with a 30 s
  lease, T1 3 s, T2 8 s. The first renew (unicast) is ACKed, the second ignored (so the guest rebinds by
  broadcast at T2, ACKed), the third is NAKed. The next DISCOVER gets 192.168.77.21 for an hour, and every later
  DISCOVER (a cable plugged back in) gets .21 again.

Usage: fake_lan.py <listen_port> <qemu_port> <log>
"""
import socket
import struct
import sys
import time

LISTEN, QEMU, LOG = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
ROUTER = bytes([192, 168, 77, 1])
MAC = bytes.fromhex("02aabbccddee")
BCAST = b"\xff" * 6
FIRST, SECOND = bytes([192, 168, 77, 20]), bytes([192, 168, 77, 21])
T0 = time.time()


def log(msg):
    with open(LOG, "a") as f:
        f.write(f"{time.time() - T0:7.3f} {msg}\n")


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


def icmp(typ, ident, seq, data):
    m = bytearray(struct.pack("!BBHHH", typ, 0, 0, ident, seq) + data)
    m[2:4] = struct.pack("!H", csum(bytes(m)))
    return bytes(m)


def dhcp_options(b):
    o, i = {}, 240
    while i < len(b):
        code = b[i]
        if code == 0:
            i += 1
            continue
        if code == 255 or i + 1 >= len(b):
            break
        n = b[i + 1]
        o[code] = b[i + 2:i + 2 + n]
        i += 2 + n
    return o


class Lan:
    def __init__(self):
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.bind(("127.0.0.1", LISTEN))
        self.sock.settimeout(0.2)
        self.discovers = 0
        self.renews = 0
        self.phase = 1  # 1: the short lease on .20; 2: .21 after the NAK
        self.guest_mac = None
        self.guest_ip = None
        self.next_ping = 0.0
        self.ping_seq = 0

    def send(self, frame):
        self.sock.sendto(frame, ("127.0.0.1", QEMU))

    def dhcp_reply(self, req, kind, ip, lease, t1, t2):
        xid, flags, ciaddr, chaddr = req[4:8], req[10:12], req[12:16], req[28:44]
        b = bytes([2, 1, 6, 0]) + xid + b"\0\0" + flags + ciaddr + (ip if kind != 6 else b"\0" * 4) + ROUTER + b"\0" * 4 + chaddr
        b += b"\0" * 192 + bytes([0x63, 0x82, 0x53, 0x63])
        opts = bytes([53, 1, kind, 54, 4]) + ROUTER
        if kind != 6:
            opts += bytes([51, 4]) + struct.pack("!I", lease) + bytes([58, 4]) + struct.pack("!I", t1) + bytes([59, 4]) + struct.pack("!I", t2)
            opts += bytes([1, 4, 255, 255, 255, 0, 3, 4]) + ROUTER + bytes([6, 4]) + ROUTER
        b += opts + bytes([255])
        dst = b"\xff" * 4
        self.send(eth(BCAST, 0x0800, ipv4(17, ROUTER, dst, udp(ROUTER, dst, 67, 68, b))))

    def lease(self):
        return (FIRST, 30, 3, 8) if self.phase == 1 else (SECOND, 3600, 1800, 3150)

    def on_dhcp(self, ip_dst, b):
        if len(b) < 240 or b[0] != 1:
            return
        o = dhcp_options(b)
        kind = o.get(53, b"\0")[0]
        ciaddr = b[12:16]
        self.guest_mac = b[28:34]
        if kind == 1:
            self.discovers += 1
            if self.phase == 1 and self.discovers <= 3:
                log(f"dhcp: DISCOVER {self.discovers} dropped")
                return
            ip, lease, t1, t2 = self.lease()
            log(f"dhcp: DISCOVER {self.discovers} -> OFFER {socket.inet_ntoa(ip)}")
            self.dhcp_reply(b, 2, ip, lease, t1, t2)
        elif kind == 3:
            ip, lease, t1, t2 = self.lease()
            if ciaddr == b"\0" * 4:
                log(f"dhcp: REQUEST (selecting) {socket.inet_ntoa(o.get(50, b'0000'))} -> ACK {socket.inet_ntoa(ip)} lease {lease}")
                self.dhcp_reply(b, 5, ip, lease, t1, t2)
                self.guest_ip = ip
            elif ip_dst == b"\xff" * 4:
                log(f"dhcp: REQUEST (rebind, broadcast) from {socket.inet_ntoa(ciaddr)} -> ACK (rebind ack)")
                self.dhcp_reply(b, 5, ip, lease, t1, t2)
            else:
                self.renews += 1
                if self.phase == 1 and self.renews == 1:
                    log("dhcp: REQUEST (renew 1, unicast) -> ACK (renew ack)")
                    self.dhcp_reply(b, 5, ip, lease, t1, t2)
                elif self.phase == 1 and self.renews == 2:
                    log("dhcp: REQUEST (renew 2, unicast) ignored")
                elif self.phase == 1:
                    log(f"dhcp: REQUEST (renew {self.renews}, unicast) -> NAK")
                    self.phase = 2
                    self.dhcp_reply(b, 6, ip, 0, 0, 0)
                else:
                    log(f"dhcp: REQUEST (renew {self.renews}) -> ACK")
                    self.dhcp_reply(b, 5, ip, lease, t1, t2)

    def on_frame(self, f):
        if len(f) < 14:
            return
        typ = struct.unpack("!H", f[12:14])[0]
        if typ == 0x0806 and len(f) >= 42 and f[20:22] == b"\x00\x01" and f[38:42] == ROUTER:
            self.send(eth(f[6:12], 0x0806, f[14:20] + b"\x00\x02" + MAC + ROUTER + f[22:28] + f[28:32]))
            return
        if typ != 0x0800 or len(f) < 34:
            return
        h = f[14:]
        ihl = (h[0] & 15) * 4
        proto, src, dst = h[9], h[12:16], h[16:20]
        data = h[ihl:struct.unpack("!H", h[2:4])[0]]
        if proto == 17 and len(data) >= 8 and struct.unpack("!H", data[2:4])[0] == 67:
            self.on_dhcp(dst, data[8:])
        elif proto == 1 and len(data) >= 8 and data[0] == 8 and dst == ROUTER:
            ident, seq = struct.unpack("!HH", data[4:8])
            log(f"icmp: echo request from {socket.inet_ntoa(src)} seq {seq} -> reply")
            self.send(eth(f[6:12], 0x0800, ipv4(1, ROUTER, src, icmp(0, ident, seq, data[8:]))))
        elif proto == 1 and len(data) >= 8 and data[0] == 0 and dst == ROUTER:
            log(f"icmp: guest answered ping seq {struct.unpack('!H', data[6:8])[0]} from {socket.inet_ntoa(src)}")

    def run(self):
        log(f"fake LAN on 127.0.0.1:{LISTEN}, QEMU at {QEMU}")
        while True:
            try:
                f, _ = self.sock.recvfrom(65536)
                self.on_frame(f)
            except socket.timeout:
                pass
            now = time.time()
            if self.phase == 2 and self.guest_ip and self.guest_mac and now >= self.next_ping:
                self.next_ping = now + 1.0
                self.ping_seq += 1
                self.send(eth(self.guest_mac, 0x0800, ipv4(1, ROUTER, self.guest_ip, icmp(8, 0x4c41, self.ping_seq, b"fake-lan"))))


Lan().run()
