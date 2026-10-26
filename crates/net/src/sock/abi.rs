//! The socket ABI of x86-64 Linux (values from linux-6.6/include/linux/socket.h,
//! include/uapi/asm-generic/socket.h, include/uapi/linux/in.h, include/uapi/linux/tcp.h, include/net/scm.h and
//! include/uapi/linux/net.h). Kept here, not in `uapi`, because only the socket layer reads them.

pub const AF_UNSPEC: u16 = 0;
pub const AF_UNIX: u16 = 1;
pub const AF_INET: u16 = 2;
pub const AF_INET6: u16 = 10;
pub const AF_NETLINK: u16 = 16;
/// `NPROTO` (= AF_MAX): families at or above it are EAFNOSUPPORT in `__sock_create`.
pub const NPROTO: u64 = 46;

pub const SOCK_STREAM: u32 = 1;
pub const SOCK_DGRAM: u32 = 2;
pub const SOCK_RAW: u32 = 3;
pub const SOCK_SEQPACKET: u32 = 5;
/// `SOCK_MAX`: types at or above it are EINVAL in `__sock_create`.
pub const SOCK_MAX: u32 = 11;
pub const SOCK_TYPE_MASK: u64 = 0xf;
pub const SOCK_NONBLOCK: u64 = 0o4000;
pub const SOCK_CLOEXEC: u64 = 0o2000000;

pub const IPPROTO_IP: u32 = 0;
pub const IPPROTO_ICMP: u32 = 1;
pub const IPPROTO_TCP: u32 = 6;
pub const IPPROTO_UDP: u32 = 17;
/// `IPPROTO_MAX`: protocols at or above it are EINVAL in `inet_create`.
pub const IPPROTO_MAX: u32 = 263;

pub const SOL_SOCKET: u32 = 1;
pub const SOL_IP: u32 = 0;
pub const SOL_TCP: u32 = 6;
pub const SOL_UDP: u32 = 17;

pub const SO_DEBUG: u32 = 1;
pub const SO_REUSEADDR: u32 = 2;
pub const SO_TYPE: u32 = 3;
pub const SO_ERROR: u32 = 4;
pub const SO_DONTROUTE: u32 = 5;
pub const SO_BROADCAST: u32 = 6;
pub const SO_SNDBUF: u32 = 7;
pub const SO_RCVBUF: u32 = 8;
pub const SO_KEEPALIVE: u32 = 9;
pub const SO_OOBINLINE: u32 = 10;
pub const SO_NO_CHECK: u32 = 11;
pub const SO_PRIORITY: u32 = 12;
pub const SO_LINGER: u32 = 13;
pub const SO_BSDCOMPAT: u32 = 14;
pub const SO_REUSEPORT: u32 = 15;
pub const SO_PASSCRED: u32 = 16;
pub const SO_PEERCRED: u32 = 17;
pub const SO_RCVLOWAT: u32 = 18;
pub const SO_SNDLOWAT: u32 = 19;
pub const SO_RCVTIMEO_OLD: u32 = 20;
pub const SO_SNDTIMEO_OLD: u32 = 21;
pub const SO_ACCEPTCONN: u32 = 30;
pub const SO_SNDBUFFORCE: u32 = 32;
pub const SO_RCVBUFFORCE: u32 = 33;
pub const SO_PROTOCOL: u32 = 38;
pub const SO_DOMAIN: u32 = 39;
pub const SO_RCVTIMEO_NEW: u32 = 66;
pub const SO_BINDTODEVICE: u32 = 25;
pub const SO_TIMESTAMP_OLD: u32 = 29;
pub const SO_TIMESTAMPNS_OLD: u32 = 35;
pub const SO_TIMESTAMPING_OLD: u32 = 37;
pub const SO_BUSY_POLL: u32 = 46;
pub const SO_INCOMING_CPU: u32 = 49;
pub const SO_PEERGROUPS: u32 = 59;
/// `sk_bound_dev_if` by index (Linux 5.1+).
pub const SO_BINDTOIFINDEX: u32 = 62;
pub const SO_TIMESTAMP_NEW: u32 = 63;
pub const SO_TIMESTAMPNS_NEW: u32 = 64;
pub const SO_TIMESTAMPING_NEW: u32 = 65;
/// IFNAMSIZ: SO_BINDTODEVICE reads at most IFNAMSIZ - 1 bytes of the name.
pub const IFNAMSIZ: usize = 16;
pub const SO_SNDTIMEO_NEW: u32 = 67;

pub const TCP_NODELAY: u32 = 1;
pub const TCP_MAXSEG: u32 = 2;
pub const TCP_CORK: u32 = 3;
pub const TCP_KEEPIDLE: u32 = 4;
pub const TCP_KEEPINTVL: u32 = 5;
pub const TCP_KEEPCNT: u32 = 6;
pub const TCP_QUICKACK: u32 = 12;

pub const SCM_RIGHTS: u32 = 1;
pub const SCM_CREDENTIALS: u32 = 2;
/// `SCM_MAX_FD`: descriptors in one SCM_RIGHTS message (a bound per message, include/net/scm.h).
pub const SCM_MAX_FD: usize = 253;

pub const MSG_OOB: u32 = 1;
pub const MSG_PEEK: u32 = 2;
pub const MSG_DONTROUTE: u32 = 4;
pub const MSG_CTRUNC: u32 = 8;
pub const MSG_TRUNC: u32 = 0x20;
pub const MSG_DONTWAIT: u32 = 0x40;
pub const MSG_EOR: u32 = 0x80;
pub const MSG_WAITALL: u32 = 0x100;
pub const MSG_ERRQUEUE: u32 = 0x2000;
pub const MSG_NOSIGNAL: u32 = 0x4000;
pub const MSG_MORE: u32 = 0x8000;
pub const MSG_WAITFORONE: u32 = 0x10000;
pub const MSG_BATCH: u32 = 0x40000;
pub const MSG_CMSG_CLOEXEC: u32 = 0x4000_0000;
pub const MSG_CMSG_COMPAT: u32 = 0x8000_0000;
/// `MSG_INTERNAL_SENDMSG_FLAGS` (MSG_SPLICE_PAGES | MSG_SENDPAGE_NOPOLICY | MSG_SENDPAGE_DECRYPTED): stripped
/// from what a program passes.
pub const MSG_INTERNAL_SENDMSG_FLAGS: u32 = 0x0800_0000 | 0x0001_0000 | 0x0010_0000;

pub const SHUT_RD: u64 = 0;
pub const SHUT_WR: u64 = 1;
pub const SHUT_RDWR: u64 = 2;
/// `sk_shutdown` bits.
pub const RCV_SHUTDOWN: u8 = 1;
pub const SEND_SHUTDOWN: u8 = 2;
pub const SHUTDOWN_MASK: u8 = 3;

/// ioctls a socket answers itself (SIOCINQ = FIONREAD, SIOCOUTQ = TIOCOUTQ).
pub const SIOCINQ: u64 = 0x541B;
pub const SIOCOUTQ: u64 = 0x5411;
/// `sock_ioctl` / protocol ioctls a socket answers without user memory beyond the int it returns.
pub const FIOGETOWN: u64 = 0x8903;
pub const SIOCGPGRP: u64 = 0x8904;
pub const SIOCATMARK: u64 = 0x8905;
/// `SIOCOUTQNSD` (TCP: bytes not sent yet).
pub const SIOCOUTQNSD: u64 = 0x894B;
pub const SIOCGSTAMP_OLD: u64 = 0x8906;
pub const SIOCGSTAMPNS_OLD: u64 = 0x8907;
/// `_IOR(0x89, 6/7, long long[2])`: the 64-bit-time variants.
pub const SIOCGSTAMP_NEW: u64 = 0x8010_8906;
pub const SIOCGSTAMPNS_NEW: u64 = 0x8010_8907;

/// `struct sockaddr_storage` (the largest address a call takes).
pub const SOCKADDR_STORAGE: usize = 128;
/// `struct sockaddr_in`.
pub const SOCKADDR_IN: usize = 16;
/// `offsetof(struct sockaddr_un, sun_path)` and `sizeof(struct sockaddr_un)`.
pub const SUN_PATH_OFF: usize = 2;
pub const SOCKADDR_UN: usize = 110;
/// `struct msghdr` (user layout), `struct mmsghdr`, `struct cmsghdr`.
pub const MSGHDR: usize = 56;
pub const MMSGHDR: usize = 64;
pub const CMSGHDR: usize = 16;
/// `UIO_MAXIOV`.
pub const UIO_MAXIOV: u64 = 1024;
/// net.core.optmem_max on CachyOS (the ABI diff's reference): the largest control buffer a sendmsg may pass.
pub const OPTMEM_MAX: u64 = 131072;
/// `MAX_RW_COUNT` (INT_MAX & PAGE_MASK).
pub const MAX_RW_COUNT: u64 = 0x7fff_f000;

/// Errno values the socket layer uses beside the common ones (INT-A2a: `uapi::errno` names them now, NW-5
/// change request; re-exported so both glob imports see one item).
pub use uapi::errno::{EDOM, ESHUTDOWN, ESOCKTNOSUPPORT, ETOOMANYREFS};

/// Signal numbers the socket layer sends.
pub const SIGPIPE: u32 = 13;

/// `CMSG_ALIGN`, `CMSG_LEN`, `CMSG_SPACE` for x86-64 (alignment = sizeof(long)).
pub const fn cmsg_align(n: usize) -> usize {
    (n + 7) & !7
}
pub const fn cmsg_len(n: usize) -> usize {
    CMSGHDR + n
}
pub const fn cmsg_space(n: usize) -> usize {
    CMSGHDR + cmsg_align(n)
}

/// The system values a program can read (CachyOS's settings, the ABI diff's oracle; Linux's own defaults are
/// smaller for `max_dgram_qlen` (10) and the `*mem_max` pair (212992)): `net.core.{w,r}mem_default`,
/// `net.core.{w,r}mem_max`, `net.core.somaxconn`, `net.unix.max_dgram_qlen`, `net.ipv4.tcp_{w,r}mem[1]`.
pub const WMEM_DEFAULT: u32 = 212_992;
pub const RMEM_DEFAULT: u32 = 212_992;
pub const WMEM_MAX: u32 = 4_194_304;
pub const RMEM_MAX: u32 = 4_194_304;
pub const SOMAXCONN: u32 = 4096;
pub const MAX_DGRAM_QLEN: usize = 512;
pub const TCP_WMEM_DEFAULT: u32 = 16_384;
pub const TCP_RMEM_DEFAULT: u32 = 131_072;
/// `SOCK_MIN_SNDBUF`, `SOCK_MIN_RCVBUF` (2 x / 1 x TCP_SKB_MIN_TRUESIZE = 2048 + 256).
pub const SOCK_MIN_SNDBUF: u32 = 4608;
pub const SOCK_MIN_RCVBUF: u32 = 2304;
/// `ip_unprivileged_port_start`.
pub const UNPRIVILEGED_PORT_START: u16 = 1024;
/// `overflowuid`/`overflowgid` (what an invalid id is shown as).
pub const OVERFLOW_ID: u32 = 65534;
