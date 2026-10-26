//! abi-c: wave C ("files and network", spec/rust-ecosystem.md §3.5): getdents64, mkdir/unlinkat/rename*/
//! link/symlink/readlink, truncate/ftruncate/pread/pwrite, fsync, utimensat, statfs, chmod, chdir/getcwd,
//! access, /dev and /proc files, and the socket calls on 127.0.0.1 (TCP and UDP) and AF_UNIX pairs. Prints a
//! transcript equal to CachyOS's.
#![forbid(unsafe_code)]

use abitest::{T, scratch};
use usys::*;

const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0o100;
const O_DIRECTORY: usize = 0o200000;
const AT_FDCWD: isize = -100;
const AT_REMOVEDIR: usize = 0x200;
const AT_SYMLINK_NOFOLLOW: usize = 0x100;
const RENAME_NOREPLACE: usize = 1;
const RENAME_EXCHANGE: usize = 2;

fn create(path: &str, text: &[u8]) -> SysResult {
    let fd = open(path, O_CREAT | O_WRONLY, 0o644)?;
    let r = write(fd, text);
    let _ = close(fd);
    r
}

fn read_text(path: &str, max: usize) -> Result<Vec<u8>, Errno> {
    let fd = open(path, O_RDONLY, 0)?;
    let mut out = Vec::new();
    let mut b = vec![0u8; 4096];
    loop {
        match read(fd, &mut b) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&b[..n]);
                if out.len() >= max {
                    break;
                }
            }
            Err(e) => {
                let _ = close(fd);
                return Err(e);
            }
        }
    }
    let _ = close(fd);
    Ok(out)
}

fn listing(dir: &str) -> String {
    let fd = match open(dir, O_RDONLY | O_DIRECTORY, 0) {
        Ok(fd) => fd,
        Err(e) => return e.to_string(),
    };
    let mut all = Vec::new();
    let mut b = vec![0u8; 256];
    loop {
        match getdents64(fd, &mut b) {
            Ok(0) => break,
            Ok(n) => all.extend(parse_dirents(&b[..n])),
            Err(e) => {
                let _ = close(fd);
                return e.to_string();
            }
        }
    }
    let _ = close(fd);
    all.sort();
    all.iter().map(|(n, t)| format!("{}/{t}", String::from_utf8_lossy(n))).collect::<Vec<_>>().join(" ")
}

fn main() {
    let mut t = T::begin('C');
    let dir = scratch('C');

    // --- names
    t.sys("c.mkdir.scratch", mkdir(&dir, 0o755));
    t.sys("c.mkdir.again", mkdir(&dir, 0o755));
    t.sys("c.mkdirat.sub", mkdirat(AT_FDCWD, &format!("{dir}/sub"), 0o700));
    t.sys("c.mkdir.missing_parent", mkdir(&format!("{dir}/no/such"), 0o755));
    for n in ["a", "b", "c"] {
        t.sys(&format!("c.create.{n}"), create(&format!("{dir}/{n}"), n.as_bytes()));
    }
    t.say("c.getdents64.listing", listing(&dir));
    t.sys("c.getdents64.on_file", open(&format!("{dir}/a"), O_RDONLY, 0).and_then(|fd| {
        let mut b = vec![0u8; 256];
        let r = getdents64(fd, &mut b);
        let _ = close(fd);
        r
    }));
    t.sys("c.rename.a_to_d", rename(&format!("{dir}/a"), &format!("{dir}/d")));
    t.sys("c.rename.missing", rename(&format!("{dir}/a"), &format!("{dir}/e")));
    t.sys("c.rename.over_existing", rename(&format!("{dir}/d"), &format!("{dir}/c")));
    t.say("c.rename.content_moved", read_text(&format!("{dir}/c"), 64).map(|v| String::from_utf8_lossy(&v).into_owned()).unwrap_or_else(|e| e.to_string()));
    t.sys("c.rename.dir_over_file", rename(&format!("{dir}/sub"), &format!("{dir}/c")));
    t.sys("c.renameat.c_to_a", renameat(AT_FDCWD, &format!("{dir}/c"), AT_FDCWD, &format!("{dir}/a")));
    t.sys("c.renameat2.noreplace_existing", renameat2(AT_FDCWD, &format!("{dir}/a"), AT_FDCWD, &format!("{dir}/b"), RENAME_NOREPLACE));
    t.sys("c.renameat2.exchange", renameat2(AT_FDCWD, &format!("{dir}/a"), AT_FDCWD, &format!("{dir}/b"), RENAME_EXCHANGE));
    t.say("c.renameat2.exchanged_content", read_text(&format!("{dir}/a"), 64).map(|v| String::from_utf8_lossy(&v).into_owned()).unwrap_or_else(|e| e.to_string()));
    t.sys("c.link.b_to_e", link(&format!("{dir}/b"), &format!("{dir}/e")));
    t.say("c.link.nlink", stat(&format!("{dir}/b")).map(|s| s.nlink.to_string()).unwrap_or_else(|e| e.to_string()));
    t.sys("c.link.existing_target", link(&format!("{dir}/b"), &format!("{dir}/e")));
    t.sys("c.link.directory", link(&format!("{dir}/sub"), &format!("{dir}/subl")));
    t.sys("c.symlink.f_to_b", symlink("b", &format!("{dir}/f")));
    let mut b = [0u8; 64];
    t.say("c.readlink.f", readlink(&format!("{dir}/f"), &mut b).map(|n| String::from_utf8_lossy(&b[..n]).into_owned()).unwrap_or_else(|e| e.to_string()));
    t.sys("c.readlink.not_a_link", readlink(&format!("{dir}/b"), &mut b));
    t.say("c.lstat.f_type", lstat(&format!("{dir}/f")).map(|s| s.file_type().to_string()).unwrap_or_else(|e| e.to_string()));
    t.say("c.stat.f_follows", stat(&format!("{dir}/f")).map(|s| s.file_type().to_string()).unwrap_or_else(|e| e.to_string()));
    t.say("c.newfstatat.nofollow", newfstatat(AT_FDCWD, &format!("{dir}/f"), AT_SYMLINK_NOFOLLOW).map(|s| s.file_type().to_string()).unwrap_or_else(|e| e.to_string()));
    t.sys("c.symlink.dangling", symlink("nowhere", &format!("{dir}/g")));
    t.kind("c.stat.dangling", &stat(&format!("{dir}/g")));
    t.say("c.getdents64.listing_after", listing(&dir));

    // --- content: truncate, pread/pwrite, sync
    let f = format!("{dir}/b");
    t.sys("c.truncate.100", truncate(&f, 100));
    t.say("c.truncate.size", stat(&f).map(|s| s.size.to_string()).unwrap_or_else(|e| e.to_string()));
    t.sys("c.truncate.negative", truncate(&f, -1));
    t.sys("c.truncate.directory", truncate(&dir, 0));
    match open(&f, O_RDWR, 0) {
        Ok(fd) => {
            t.sys("c.ftruncate.5000", ftruncate(fd, 5000));
            t.say("c.ftruncate.size", fstat(fd).map(|s| s.size.to_string()).unwrap_or_else(|e| e.to_string()));
            t.sys("c.ftruncate.negative", ftruncate(fd, -5));
            t.sys("c.pwrite64.at_4000", pwrite64(fd, b"XYZ", 4000));
            let mut r = [0u8; 8];
            t.sys("c.pread64.at_3998", pread64(fd, &mut r, 3998));
            t.say("c.pread64.content", format!("{:?}", &r[..5]));
            t.sys("c.pread64.past_end", pread64(fd, &mut r, 9000));
            t.sys("c.pread64.negative_offset", pread64(fd, &mut r, -1));
            t.sys("c.lseek.unchanged_by_pread", lseek(fd, 0, 1));
            t.sys("c.fsync", fsync(fd));
            t.sys("c.fdatasync", fdatasync(fd));
            t.sys("c.fchmod.600", fchmod(fd, 0o600));
            t.say("c.fchmod.mode", fstat(fd).map(|s| format!("{:o}", s.mode & 0o7777)).unwrap_or_else(|e| e.to_string()));
            let uid = geteuid().unwrap_or(0) as u32;
            let gid = getegid().unwrap_or(0) as u32;
            t.sys("c.fchown.to_self", fchown(fd, uid, gid));
            t.kind("c.fstatfs", &fstatfs(fd));
            let _ = close(fd);
        }
        Err(e) => t.say("c.open.b", e),
    }
    t.sys("c.chmod.644", chmod(&f, 0o644));
    t.sys("c.utimensat.set", utimensat(AT_FDCWD, Some(&f), Some([(1_000_000_000, 0), (1_234_567_890, 500)]), 0));
    t.say("c.utimensat.mtime", stat(&f).map(|s| format!("{:?}", s.mtime)).unwrap_or_else(|e| e.to_string()));
    t.sys("c.utimensat.bad_nsec", utimensat(AT_FDCWD, Some(&f), Some([(0, 2_000_000_000), (0, 0)]), 0));
    t.say("c.statfs.tmp_type", statfs("/tmp").map(|(ty, _)| format!("{ty:#x}")).unwrap_or_else(|e| e.to_string()));
    t.say("c.statfs.proc_type", statfs("/proc").map(|(ty, _)| format!("{ty:#x}")).unwrap_or_else(|e| e.to_string()));

    // --- access, cwd
    t.sys("c.access.read", access(&f, 4));
    t.sys("c.access.exec_denied", access(&f, 1));
    t.sys("c.access.missing", access(&format!("{dir}/zz"), 0));
    t.sys("c.faccessat.dir_search", faccessat(AT_FDCWD, &dir, 1));
    t.sys("c.chdir.scratch", chdir(&dir));
    let mut cwd = [0u8; 512];
    t.check("c.getcwd.is_scratch", getcwd(&mut cwd).is_ok_and(|n| cwd[..n.saturating_sub(1)] == *dir.as_bytes()));
    t.sys("c.getcwd.small_buffer", getcwd(&mut cwd[..4]));
    t.say("c.stat.relative", stat("b").map(|s| s.size.to_string()).unwrap_or_else(|e| e.to_string()));
    t.sys("c.chdir.file", chdir(&f));
    match open("/", O_RDONLY | O_DIRECTORY, 0) {
        Ok(fd) => {
            t.sys("c.fchdir.root", fchdir(fd));
            t.check("c.getcwd.root", getcwd(&mut cwd).is_ok_and(|n| &cwd[..n] == b"/\0"));
            let _ = close(fd);
        }
        Err(e) => t.say("c.open.root", e),
    }

    // --- removal
    t.sys("c.rmdir.not_empty", rmdir(&dir));
    t.sys("c.unlinkat.removedir_not_empty", unlinkat(AT_FDCWD, &dir, AT_REMOVEDIR));
    t.sys("c.unlink.directory", unlink(&format!("{dir}/sub")));
    t.sys("c.rmdir.file", rmdir(&f));
    t.sys("c.unlinkat.sub", unlinkat(AT_FDCWD, &format!("{dir}/sub"), AT_REMOVEDIR));
    for n in ["a", "b", "e", "f", "g"] {
        t.sys(&format!("c.unlink.{n}"), unlink(&format!("{dir}/{n}")));
    }
    t.sys("c.rmdir.scratch", rmdir(&dir));

    // --- devices and /proc
    match open("/dev/null", O_RDWR, 0) {
        Ok(fd) => {
            t.sys("c.dev_null.write", write(fd, b"gone"));
            let mut b = [0u8; 4];
            t.sys("c.dev_null.read", read(fd, &mut b));
            let _ = close(fd);
        }
        Err(e) => t.say("c.dev_null", e),
    }
    t.say("c.dev_zero.read", read_text("/dev/zero", 16).map(|v| format!("{} zero {}", v.len().min(16), v.iter().take(16).all(|b| *b == 0))).unwrap_or_else(|e| e.to_string()));
    match open("/dev/urandom", O_RDONLY, 0) {
        Ok(fd) => {
            let mut b = [0u8; 16];
            t.sys("c.dev_urandom.read", read(fd, &mut b));
            let _ = close(fd);
        }
        Err(e) => t.say("c.dev_urandom", e),
    }
    match open("/dev/full", O_WRONLY, 0) {
        Ok(fd) => {
            t.sys("c.dev_full.write", write(fd, b"x"));
            let _ = close(fd);
        }
        Err(e) => t.say("c.dev_full", e),
    }
    let pid = getpid().unwrap_or(0);
    t.check("c.proc_self.stat_pid", read_text("/proc/self/stat", 4096).is_ok_and(|v| String::from_utf8_lossy(&v).split(' ').next() == Some(&pid.to_string())));
    let mut l = [0u8; 32];
    t.check("c.proc_self.link_is_pid", readlink("/proc/self", &mut l).is_ok_and(|n| l[..n] == *pid.to_string().as_bytes()));
    t.check("c.proc_self.exe_ends_abi_c", {
        let mut b = [0u8; 512];
        readlink("/proc/self/exe", &mut b).is_ok_and(|n| b[..n].ends_with(b"/abi-c"))
    });
    t.check("c.proc_self.cmdline_nul_terminated", read_text("/proc/self/cmdline", 4096).is_ok_and(|v| v.ends_with(b"\0")));
    t.check("c.proc.meminfo_first_line", read_text("/proc/meminfo", 4096).is_ok_and(|v| v.starts_with(b"MemTotal:")));
    t.check("c.proc.cpuinfo_processor", read_text("/proc/cpuinfo", 1 << 16).is_ok_and(|v| v.starts_with(b"processor\t: 0\n")));
    t.check("c.proc.uptime_two_numbers", read_text("/proc/uptime", 256).is_ok_and(|v| String::from_utf8_lossy(&v).split_whitespace().filter(|w| w.parse::<f64>().is_ok()).count() == 2));

    // --- TCP on 127.0.0.1
    let lo = [127, 0, 0, 1];
    let srv = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
    t.kind("c.socket.tcp", &srv);
    if let Ok(s) = srv {
        t.sys("c.setsockopt.reuseaddr", setsockopt_int(s, SOL_SOCKET, SO_REUSEADDR, 1));
        t.say("c.getsockopt.reuseaddr", getsockopt_int(s, SOL_SOCKET, SO_REUSEADDR).map(|v| v.to_string()).unwrap_or_else(|e| e.to_string()));
        t.say("c.getsockopt.type", getsockopt_int(s, SOL_SOCKET, SO_TYPE).map(|v| v.to_string()).unwrap_or_else(|e| e.to_string()));
        t.sys("c.bind.loopback_any_port", bind_inet(s, lo, 0));
        let name = getsockname_inet(s);
        t.say("c.getsockname.loopback_port_set", name.map(|(ip, p)| format!("{ip:?} {}", p != 0)).unwrap_or_else(|e| e.to_string()));
        t.sys("c.listen", listen(s, 4));
        let port = name.map(|(_, p)| p).unwrap_or(0);
        match socket(AF_INET, SOCK_STREAM, 0) {
            Ok(c) => {
                t.sys("c.connect.loopback", connect_inet(c, lo, port));
                let acc = accept4(s, SOCK_CLOEXEC);
                t.say("c.accept4.peer", acc.map(|(_, ip, _)| format!("{ip:?}")).unwrap_or_else(|e| e.to_string()));
                if let Ok((a, _, pport)) = acc {
                    t.check("c.getpeername.matches_client", getsockname_inet(c).is_ok_and(|(_, p)| p == pport) && getpeername_inet(c).is_ok_and(|(_, p)| p == port));
                    t.sys("c.setsockopt.nodelay", setsockopt_int(c, IPPROTO_TCP, TCP_NODELAY, 1));
                    t.say("c.getsockopt.nodelay", getsockopt_int(c, IPPROTO_TCP, TCP_NODELAY).map(|v| (v != 0).to_string()).unwrap_or_else(|e| e.to_string()));
                    t.sys("c.send.ping", sendto(c, b"ping", 0, None));
                    let mut b = [0u8; 16];
                    t.say("c.recv.ping", recvfrom(a, &mut b, 0).map(|(n, _, _)| String::from_utf8_lossy(&b[..n]).into_owned()).unwrap_or_else(|e| e.to_string()));
                    t.sys("c.sendmsg.pong", sendmsg(a, b"pong", 0));
                    t.say("c.recvmsg.pong", recvmsg(c, &mut b, 0).map(|n| String::from_utf8_lossy(&b[..n]).into_owned()).unwrap_or_else(|e| e.to_string()));
                    t.sys("c.recv.nothing_dontwait", recvfrom(c, &mut b, 0x40).map(|(n, _, _)| n));
                    t.sys("c.shutdown.write", shutdown(c, 1));
                    t.sys("c.recv.after_peer_shutdown", recvfrom(a, &mut b, 0).map(|(n, _, _)| n));
                    t.say("c.getsockopt.error", getsockopt_int(a, SOL_SOCKET, SO_ERROR).map(|v| v.to_string()).unwrap_or_else(|e| e.to_string()));
                    let _ = close(a);
                }
                let _ = close(c);
            }
            Err(e) => t.say("c.socket.client", e),
        }
        let _ = close(s);
        // Nobody listens there any more.
        match socket(AF_INET, SOCK_STREAM, 0) {
            Ok(c) => {
                t.sys("c.connect.refused", connect_inet(c, lo, port));
                let _ = close(c);
            }
            Err(e) => t.say("c.connect.refused", e),
        }
    }
    t.kind("c.socket.bad_family", &socket(99, SOCK_STREAM, 0));
    t.kind("c.socket.bad_type", &socket(AF_INET, 99, 0));
    t.sys("c.listen.not_a_socket", listen(0, 1));

    // --- UDP on 127.0.0.1
    match socket(AF_INET, SOCK_DGRAM, 0) {
        Ok(u) => {
            t.sys("c.bind.udp", bind_inet(u, lo, 0));
            let port = getsockname_inet(u).map(|(_, p)| p).unwrap_or(0);
            t.sys("c.sendto.udp_self", sendto(u, b"hello", 0, Some((lo, port))));
            let mut b = [0u8; 16];
            t.say("c.recvfrom.udp_self", recvfrom(u, &mut b, 0).map(|(n, ip, p)| format!("{} {ip:?} same_port {}", String::from_utf8_lossy(&b[..n]), p == port)).unwrap_or_else(|e| e.to_string()));
            t.sys("c.recvfrom.udp_empty_dontwait", recvfrom(u, &mut b, 0x40).map(|(n, _, _)| n));
            let _ = close(u);
        }
        Err(e) => t.say("c.socket.udp", e),
    }

    // --- AF_UNIX pair
    match socketpair(AF_UNIX, SOCK_STREAM) {
        Ok((x, y)) => {
            t.sys("c.socketpair.write", write(x, b"abc"));
            let mut b = [0u8; 8];
            t.say("c.socketpair.read", read(y, &mut b).map(|n| String::from_utf8_lossy(&b[..n]).into_owned()).unwrap_or_else(|e| e.to_string()));
            t.sys("c.socketpair.back", write(y, b"de"));
            t.sys("c.socketpair.read_back", read(x, &mut b));
            let _ = close(x);
            t.sys("c.socketpair.peer_closed_read", read(y, &mut b));
            let _ = close(y);
        }
        Err(e) => t.say("c.socketpair", e),
    }
    t.kind("c.socketpair.inet_unsupported", &socketpair(AF_INET, SOCK_STREAM));

    t.end();
}
