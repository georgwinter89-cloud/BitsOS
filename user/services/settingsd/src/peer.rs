//! Who a request comes from (CL-S). The answer is the kernel's, never the message's: `SO_PEERCRED` gives the
//! pid and uid of the process that connected, and the kernel's record of the program that called `connect`
//! (BitsOS: `SO_PEERSEC`, [`recorded`]) the program. Both were taken by the kernel at `connect`, so nothing
//! the peer does afterwards (exec another program, hand the socket on, exit and leave its pid to another)
//! changes the answer. They are read once, right after `accept`, and kept for the connection.
//!
//! A kernel without that record (Linux, where the tests run) is asked through `/proc/<pid>/exe` instead
//! ([`running`]): that names the program the pid runs when settingsd looks, with room between `connect` and
//! the look. It is the fallback only; a record that is there is never second-guessed by `/proc`.
//!
//! Two steps, because the accept loop serves everyone (B4). [`seen`] runs there and asks only what the kernel
//! holds in memory. [`Seen::settle`] runs in the client's own thread and asks the file system what lies at
//! the program's path; a stick that hangs then holds up that one client. Nothing about the pid is asked in
//! the second step.
//!
//! A program is only told when it lives where settingsd lives: in the same user and mount namespace (CL-T).
//! Whoever makes a user namespace has every capability over what runs in it (it reads and writes the owner's
//! program however that one closed its memory), and whoever makes a mount namespace decides which files the
//! owner's program finds under its fixed paths (certificates, name lookup, sockets). The file such a program
//! runs is the owner's, the program is somebody's puppet: it gets no secret.
//!
//! What this cannot see: who drives the owner's program (its
//! terminal, its arguments).

use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;

/// Where a peer's program was learnt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The kernel's record of the program that called `connect`.
    Kernel,
    /// `/proc/<pid>` right after `accept`: a kernel without that record.
    Proc,
    /// Not asked of a socket: the MCP bridge, a peer the caller vouches for.
    Given,
}

/// A program as an owner of settings: the uid it ran as and its executable as the kernel resolved it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ident {
    pub uid: u32,
    pub exe: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    /// For the log only: by the time anyone reads it the number may be someone else's.
    pub pid: u32,
    pub uid: Option<u32>,
    pub exe: Option<String>,
    pub by: Source,
}

/// A caller inside settingsd itself (the MCP bridge): it has no program of its own, so no secret is its.
pub static INSIDE: Peer = Peer { pid: 0, uid: None, exe: None, by: Source::Given };

impl Peer {
    /// A peer the caller vouches for: tests, and code that holds a registry in its own process.
    pub fn program(uid: u32, exe: &str) -> Peer {
        Peer { pid: 0, uid: Some(uid), exe: Some(exe.to_string()), by: Source::Given }
    }

    /// A program on the socket that settingsd could not tell (it was gone, or not the file at its path, by
    /// the time settingsd looked): not the same as a program that is known and is not the owner.
    pub fn untold(&self) -> bool {
        self.uid.is_some() && self.exe.is_none()
    }

    pub fn ident(&self) -> Option<Ident> {
        Some(Ident { uid: self.uid?, exe: self.exe.clone()? })
    }

    /// For `status`: what settingsd takes the asker for.
    pub fn json(&self) -> crate::json::Json {
        let by = match self.by {
            Source::Kernel => "kernel",
            Source::Proc => "proc",
            Source::Given => "-",
        };
        crate::json::Json::obj().with("pid", u64::from(self.pid)).with("uid", self.uid.map(u64::from)).with("exe", self.exe.clone()).with("by", by)
    }

    /// For event lines: `pid=.. uid=.. exe=..` (never a value), and for a peer on the socket how its program
    /// was learnt (`by=kernel`, `by=proc`).
    pub fn show(&self) -> String {
        let uid = self.uid.map_or_else(|| "-".to_string(), |u| u.to_string());
        let by = match self.by {
            Source::Kernel => " by=kernel",
            Source::Proc => " by=proc",
            Source::Given => "",
        };
        format!("pid={} uid={uid} exe={}{by}", self.pid, crate::events::q(self.exe.as_deref().unwrap_or("-")))
    }
}

/// What every label of BitsOS's kernel starts with (kernel: `proc::peer::LABEL` without its version).
const LABEL: &str = "bitsos:program:";

/// The kernel's record of the program that called `connect`, from its label
/// `bitsos:program:1:<init|own>:<st_dev>:<st_ino>:<path>` (the kernel's `proc::peer` writes it; the path is
/// last and may hold any byte). `None`: not such a label, the kernel has no record to give (the caller asks
/// `/proc`). `Some(None)`: the kernel answered, and names nobody settingsd can tell: a program in a user or
/// mount namespace of its own (the module's text says why that is nobody), one without a file or whose file
/// has no name in the boot's view, or a label of a version this settingsd does not read. `/proc` is not
/// asked then.
pub fn recorded(label: &[u8]) -> Option<Option<Runs>> {
    let rest = label.strip_prefix(LABEL.as_bytes())?;
    let runs = || {
        let mut f = rest.splitn(5, |&b| b == b':');
        let (version, ns, dev, ino, path) = (f.next()?, f.next()?, f.next()?, f.next()?, f.next()?);
        let num = |b: &[u8]| std::str::from_utf8(b).ok()?.parse::<u64>().ok();
        let path = std::str::from_utf8(path).ok()?;
        (version == b"1" && ns == b"init" && path.starts_with('/'))
            .then_some(Runs { path: path.to_string(), dev: num(dev)?, ino: num(ino)? })
    };
    Some(runs())
}

/// The file a process runs, as `/proc` shows it: the path the kernel names and the file behind the link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Runs {
    pub path: String,
    pub dev: u64,
    pub ino: u64,
}

impl Runs {
    /// The path, when it names, here, the very file the process runs. `/proc/<pid>/exe` leads to that file
    /// itself, so a program started in a mount namespace or root of its own (bwrap, chroot) that has some
    /// other file under the owner's path is not taken for the owner; neither is a removed file. `at` is the
    /// file system's answer for a path (device, inode) and may wait for a disk.
    pub fn told(self, at: impl FnOnce(&str) -> Option<(u64, u64)>) -> Option<String> {
        (at(&self.path)? == (self.dev, self.ino)).then_some(self.path)
    }
}

/// The file at `path` in settingsd's own view. Walks the path: the stick may be asked, and may hang.
pub fn on_disk(path: &str) -> Option<(u64, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.dev(), m.ino()))
}

/// What the lookup needs from `/proc`; tests bring their own to play a pid that changes hands. Both answers
/// come from the kernel's memory: the accept loop asks them.
pub trait Procs {
    /// Field 22 of `/proc/<pid>/stat`: when the process started, in clock ticks since boot.
    fn start(&self, pid: u32) -> Option<u64>;
    fn runs(&self, pid: u32) -> Option<Runs>;
    /// Whether the process lives in settingsd's own user and mount namespace. False also when the kernel
    /// does not say (the process is gone, or its links are not settingsd's to read).
    fn native(&self, pid: u32) -> bool;
}

pub struct ProcFs;

impl Procs for ProcFs {
    fn start(&self, pid: u32) -> Option<u64> {
        let t = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The name (field 2) may hold spaces and brackets; the fields after its last `)` cannot.
        t.rsplit_once(')')?.1.split_ascii_whitespace().nth(19)?.parse().ok()
    }
    /// `stat` through the magic link reaches the file the process holds open, without a walk over its path.
    fn runs(&self, pid: u32) -> Option<Runs> {
        let link = format!("/proc/{pid}/exe");
        let file = std::fs::metadata(&link).ok()?;
        let path = std::fs::read_link(&link).ok()?.into_os_string().into_string().ok()?;
        Some(Runs { path, dev: file.dev(), ino: file.ino() })
    }
    /// `/proc/<pid>/ns/<kind>` names a namespace as `kind:[number]`; the same text means the same namespace.
    fn native(&self, pid: u32) -> bool {
        let named = |who: &str, kind: &str| std::fs::read_link(format!("/proc/{who}/ns/{kind}")).ok();
        ["user", "mnt"].iter().all(|kind| {
            let theirs = named(&pid.to_string(), kind);
            theirs.is_some() && theirs == named("self", kind)
        })
    }
}

/// What `pid` runs, or None when it cannot be told for sure, or runs in a namespace of its own (the module's
/// text says why that is nobody). The start time is read before and after: a pid
/// is only the same process while its start time stays (a number given out again after an exit starts
/// later), so a pid that changes hands in between names nobody.
pub fn running(procs: &dyn Procs, pid: u32) -> Option<Runs> {
    let before = procs.start(pid)?;
    let runs = procs.runs(pid)?;
    let native = procs.native(pid);
    if procs.start(pid)? != before {
        return None;
    }
    (native && runs.path.starts_with('/')).then_some(runs)
}

/// A connection as far as the accept loop looks at it.
pub struct Seen {
    pid: u32,
    uid: Option<u32>,
    runs: Option<Runs>,
    by: Source,
}

/// What the kernel says about a socket's peer; tests bring their own to play a kernel that keeps the record.
pub trait Socket {
    /// `SO_PEERCRED`: (pid, uid) as recorded at connect; None when the socket has no peer.
    fn cred(&self) -> Option<(u32, u32)>;
    /// `SO_PEERSEC`: the label recorded at connect; None when the kernel has none.
    fn label(&self) -> Option<Vec<u8>>;
}

impl Socket for UnixStream {
    fn cred(&self) -> Option<(u32, u32)> {
        let fd = usize::try_from(self.as_raw_fd()).ok()?;
        // No peer: pid 0 and ids -1 (linux/net/core/sock.c `cred_to_ucred`).
        usys::peer_cred(fd).ok().filter(|&(pid, uid, _)| pid != 0 && pid <= i32::MAX as u32 && uid != u32::MAX).map(|(pid, uid, _)| (pid, uid))
    }
    fn label(&self) -> Option<Vec<u8>> {
        usys::peer_label(usize::try_from(self.as_raw_fd()).ok()?).ok()
    }
}

/// The peer of a connection just accepted: the part that never waits for a disk.
pub fn seen(s: &UnixStream) -> Seen {
    seen_by(s, &ProcFs)
}

/// [`seen`] with the kernel's answers given. The record of `connect` decides where there is one; only
/// without one is `/proc` asked what the pid runs now.
pub fn seen_by(s: &dyn Socket, procs: &dyn Procs) -> Seen {
    let Some((pid, uid)) = s.cred() else { return Seen { pid: 0, uid: None, runs: None, by: Source::Given } };
    match s.label().as_deref().and_then(recorded) {
        Some(runs) => Seen { pid, uid: Some(uid), runs, by: Source::Kernel },
        None => Seen { pid, uid: Some(uid), runs: running(procs, pid), by: Source::Proc },
    }
}

impl Seen {
    /// The uid the kernel recorded at connect (None: no record): an accept loop can turn a stranger away at once.
    pub fn uid(&self) -> Option<u32> {
        self.uid
    }

    /// For the log only, as [`Peer::pid`].
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The part that asks the file system; for the client's own thread, before its first request.
    pub fn settle(self) -> Peer {
        self.settle_by(on_disk)
    }

    pub fn settle_by(self, at: impl FnOnce(&str) -> Option<(u64, u64)>) -> Peer {
        Peer { pid: self.pid, uid: self.uid, exe: self.runs.and_then(|r| r.told(at)), by: self.by }
    }
}

/// Both steps at once, where nobody else waits.
pub fn of(s: &UnixStream) -> Peer {
    seen(s).settle()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn file(path: &str) -> Runs {
        Runs { path: path.to_string(), dev: 1, ino: 1 }
    }

    /// Both steps against the real `/proc` and file system.
    fn program(pid: u32) -> Option<String> {
        running(&ProcFs, pid)?.told(on_disk)
    }

    /// A copy of `sleep` under `name`, running. Another test's fork may still hold the copy open for writing
    /// until its own exec; the kernel then refuses to run the file ("text file busy"), for a moment.
    fn sleeper(dir: &std::path::Path, name: &str) -> (std::path::PathBuf, std::process::Child) {
        let sleep = ["/usr/bin/sleep", "/bin/sleep"].into_iter().find(|p| std::path::Path::new(p).exists()).unwrap();
        std::fs::create_dir_all(dir).unwrap();
        let copy = dir.join(name);
        std::fs::copy(sleep, &copy).unwrap();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match std::process::Command::new(&copy).arg("30").spawn() {
                Ok(child) => return (copy, child),
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && std::time::Instant::now() < end => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => panic!("{}: {e}", copy.display()),
            }
        }
    }

    /// A pid whose process is replaced after `calls` questions.
    struct Reused {
        calls: Cell<u32>,
        swap_at: u32,
    }

    impl Reused {
        fn tick(&self) -> bool {
            self.calls.set(self.calls.get() + 1);
            self.calls.get() > self.swap_at
        }
    }

    impl Procs for Reused {
        fn start(&self, _: u32) -> Option<u64> {
            Some(if self.tick() { 900 } else { 100 })
        }
        fn runs(&self, _: u32) -> Option<Runs> {
            Some(file(if self.tick() { "/bin/owner" } else { "/tmp/stranger" }))
        }
        fn native(&self, _: u32) -> bool {
            true
        }
    }

    #[test]
    fn a_pid_that_changes_hands_during_the_lookup_names_nobody() {
        // Unchanged: the program is told.
        assert_eq!(running(&Reused { calls: Cell::new(0), swap_at: 9 }, 7), Some(file("/tmp/stranger")));
        // The stranger exits and the owner's program gets its pid between any two of the three reads.
        for swap_at in [1, 2] {
            assert_eq!(running(&Reused { calls: Cell::new(0), swap_at }, 7), None, "swap after read {swap_at}");
        }
    }

    #[test]
    fn a_process_that_is_gone_names_nobody() {
        struct Gone;
        impl Procs for Gone {
            fn start(&self, _: u32) -> Option<u64> {
                None
            }
            fn runs(&self, _: u32) -> Option<Runs> {
                Some(file("/bin/owner"))
            }
            fn native(&self, _: u32) -> bool {
                true
            }
        }
        assert_eq!(running(&Gone, 7), None);
        assert_eq!(running(&ProcFs, u32::MAX), None);
    }

    /// A program whose file is no longer the one at its path (removed, replaced, or another file under that
    /// name in a mount namespace of its own) is nobody: a copy of `sleep` that is removed while it runs.
    #[test]
    fn a_program_that_is_not_the_file_at_its_path_names_nobody() {
        let dir = std::env::temp_dir().join(format!("cls-peer-{}", std::process::id()));
        let (copy, mut child) = sleeper(&dir, "owner");
        let told = |want: Option<&str>| {
            // The child is this path's program only once its exec is done.
            let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while program(child.id()).as_deref() != want && std::time::Instant::now() < end {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            program(child.id())
        };
        assert_eq!(told(copy.to_str()).as_deref(), copy.to_str());
        // Another file takes the path: the running program is not it.
        std::fs::remove_file(&copy).unwrap();
        assert_eq!(told(None), None);
        std::fs::write(&copy, b"another file").unwrap();
        assert_eq!(program(child.id()), None);
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two real programs and one number: the questions about `pid` are answered for `first` until `swap_at`
    /// of them were asked, then for `then`, as when the first exits and the second is given its pid.
    struct Handed {
        first: u32,
        then: u32,
        calls: Cell<u32>,
        swap_at: u32,
    }

    impl Handed {
        fn now(&self) -> u32 {
            self.calls.set(self.calls.get() + 1);
            if self.calls.get() > self.swap_at { self.then } else { self.first }
        }
    }

    impl Procs for Handed {
        fn start(&self, _: u32) -> Option<u64> {
            ProcFs.start(self.now())
        }
        fn runs(&self, _: u32) -> Option<Runs> {
            ProcFs.runs(self.now())
        }
        fn native(&self, _: u32) -> bool {
            true
        }
    }

    /// The kernel's own answers for two living processes: a stranger and, started after it, the owner's
    /// program. Whenever the number passes from one to the other during the lookup, nobody is named. (A real
    /// wrap of the pid space is not forced here; what is real are the start times and the files.)
    #[test]
    fn a_pid_handed_from_one_real_program_to_another_names_nobody() {
        let dir = std::env::temp_dir().join(format!("cls-handed-{}", std::process::id()));
        let (other, mut stranger) = sleeper(&dir, "stranger");
        // A start time counts in clock ticks (10 ms): the owner's program starts in a later one.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let (owner, mut real) = sleeper(&dir, "owner");
        // Each is its path's program only once its exec is done.
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let both = || program(stranger.id()).as_deref() == other.to_str() && program(real.id()).as_deref() == owner.to_str();
        while !both() && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let lookup = |swap_at| {
            let h = Handed { first: stranger.id(), then: real.id(), calls: Cell::new(0), swap_at };
            running(&h, 7).and_then(|r| r.told(on_disk))
        };
        let own = lookup(0);
        let strange = lookup(9);
        let handed = [lookup(1), lookup(2)];
        let _ = stranger.kill();
        let _ = real.kill();
        let _ = stranger.wait();
        let _ = real.wait();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(own.as_deref(), owner.to_str());
        assert_eq!(strange.as_deref(), other.to_str());
        assert_eq!(handed, [None, None]);
    }

    /// The owner's program in a namespace of its own is nobody: whoever made the namespace reads and writes
    /// the program, or chose the files it finds. First with a kernel that says so, then for real: a copy of
    /// `sleep` started in a new user and mount namespace (`unshare`), beside one started plainly.
    #[test]
    fn a_program_in_a_namespace_of_its_own_names_nobody() {
        struct Apart;
        impl Procs for Apart {
            fn start(&self, _: u32) -> Option<u64> {
                Some(100)
            }
            fn runs(&self, _: u32) -> Option<Runs> {
                Some(file("/bin/owner"))
            }
            fn native(&self, _: u32) -> bool {
                false
            }
        }
        assert_eq!(running(&Apart, 7), None);

        let dir = std::env::temp_dir().join(format!("cls-apart-{}", std::process::id()));
        let (copy, mut plain) = sleeper(&dir, "owner");
        let mut apart = std::process::Command::new("unshare").args(["--user", "--mount"]).arg(&copy).arg("30").spawn().expect("unshare (util-linux) starts");
        // `unshare` is the copy only once its exec is done; by then it has left for its namespaces.
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let is_copy = |pid: u32| ProcFs.runs(pid).is_some_and(|r| Some(r.path.as_str()) == copy.to_str());
        while !(is_copy(plain.id()) && is_copy(apart.id())) && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let seen = (is_copy(apart.id()), ProcFs.native(plain.id()), ProcFs.native(apart.id()), program(plain.id()), program(apart.id()));
        for c in [&mut plain, &mut apart] {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(seen, (true, true, false, copy.to_str().map(str::to_string), None));
    }

    /// The accept loop's step asks the file system nothing, and the client's step nothing about the pid: a
    /// process that is gone by then is still the program it was when settingsd looked.
    #[test]
    fn the_second_step_asks_only_the_file_system() {
        let seen = Seen { pid: 7, uid: Some(1000), runs: Some(file("/bin/owner")), by: Source::Proc };
        let asked = Cell::new(0);
        let at = |p: &str| {
            asked.set(asked.get() + 1);
            (p == "/bin/owner").then_some((1, 1))
        };
        assert_eq!(seen.settle_by(at), Peer { pid: 7, uid: Some(1000), exe: Some("/bin/owner".into()), by: Source::Proc });
        assert_eq!(asked.get(), 1);
        // Another file lies at the path, or none: a program on the socket that cannot be told.
        for other in [Some((1, 2)), Some((2, 1)), None] {
            let p = Seen { pid: 7, uid: Some(1000), runs: Some(file("/bin/owner")), by: Source::Proc }.settle_by(|_| other);
            assert!(p.untold() && p.ident().is_none(), "{other:?}");
        }
        assert!(!INSIDE.untold());
    }

    /// The label as BitsOS's kernel writes it (crates/proc/src/peer.rs; its ktest `mc.local.user_record`
    /// holds the same text for a real /bin/assist).
    #[test]
    fn the_kernels_label_is_read() {
        let runs = |path: &str, dev, ino| Some(Some(Runs { path: path.into(), dev, ino }));
        assert_eq!(recorded(b"bitsos:program:1:init:2049:77:/bin/assist"), runs("/bin/assist", 2049, 77));
        // The path is last and may hold anything.
        assert_eq!(recorded(b"bitsos:program:1:init:5:6:/data/a:b c/x"), runs("/data/a:b c/x", 5, 6));
        // The kernel's answer, and nobody: a namespace of its own, no file, no name in the boot's view,
        // a path that is not text, numbers that are none, a version from the future.
        for nobody in [
            &b"bitsos:program:1:own:2049:77:/bin/assist"[..],
            b"bitsos:program:1:init:0:0:",
            b"bitsos:program:1:init:2049:77:",
            b"bitsos:program:1:init:2049:77:bin/assist",
            b"bitsos:program:1:init:2049:77:/bin/\xff",
            b"bitsos:program:1:init:x:77:/bin/assist",
            b"bitsos:program:1:init:2049:-1:/bin/assist",
            b"bitsos:program:1:INIT:2049:77:/bin/assist",
            b"bitsos:program:1:init:2049",
            b"bitsos:program:2:init:2049:77:/bin/assist",
            b"bitsos:program:",
        ] {
            assert_eq!(recorded(nobody), Some(None), "{}", String::from_utf8_lossy(nobody));
        }
        // Not this kernel's label (another system's security label, nothing): the caller asks /proc.
        for other in [&b"unconfined"[..], b"system_u:system_r:init_t:s0", b"", b"bitsos:program", b"/bin/assist"] {
            assert_eq!(recorded(other), None, "{}", String::from_utf8_lossy(other));
        }
    }

    /// A socket whose kernel keeps the record.
    struct Recorded {
        cred: Option<(u32, u32)>,
        label: Option<&'static [u8]>,
    }

    impl Socket for Recorded {
        fn cred(&self) -> Option<(u32, u32)> {
            self.cred
        }
        fn label(&self) -> Option<Vec<u8>> {
            self.label.map(<[u8]>::to_vec)
        }
    }

    /// What `/proc` would say about the pid by now: the owner's program (the stranger exec'ed it after its
    /// connect, or the owner got the stranger's pid). Any question to it is counted.
    struct NowOwner(Cell<u32>);

    impl Procs for NowOwner {
        fn start(&self, _: u32) -> Option<u64> {
            self.0.set(self.0.get() + 1);
            Some(100)
        }
        fn runs(&self, _: u32) -> Option<Runs> {
            self.0.set(self.0.get() + 1);
            Some(file("/bin/owner"))
        }
        fn native(&self, _: u32) -> bool {
            self.0.set(self.0.get() + 1);
            true
        }
    }

    /// Connect, then become the owner's program (or leave the pid to it): the kernel recorded who called
    /// `connect`, and that is the answer. `/proc`, which would name the owner by now, is not even asked.
    /// Without a record it is asked, as before.
    #[test]
    fn the_record_of_connect_decides_not_what_the_pid_runs_now() {
        let at = |p: &str| (p == "/bin/owner" || p == "/tmp/stranger").then_some((1, 1));
        let told = |label: Option<&'static [u8]>| {
            let procs = NowOwner(Cell::new(0));
            let p = seen_by(&Recorded { cred: Some((7, 1000)), label }, &procs).settle_by(at);
            (p.exe, p.by, procs.0.get() > 0)
        };
        let s = |t: &str| Some(t.to_string());
        assert_eq!(told(Some(b"bitsos:program:1:init:1:1:/tmp/stranger")), (s("/tmp/stranger"), Source::Kernel, false));
        assert_eq!(told(Some(b"bitsos:program:1:init:1:1:/bin/owner")), (s("/bin/owner"), Source::Kernel, false));
        // The owner's program, started in a namespace somebody made; and a record nobody can read.
        assert_eq!(told(Some(b"bitsos:program:1:own:1:1:/bin/owner")), (None, Source::Kernel, false));
        assert_eq!(told(Some(b"bitsos:program:9:init:1:1:/bin/owner")), (None, Source::Kernel, false));
        // The file at the recorded path is another one in settingsd's view.
        assert_eq!(told(Some(b"bitsos:program:1:init:1:2:/bin/owner")), (None, Source::Kernel, false));
        // No record (Linux), or another system's label: /proc is all there is.
        assert_eq!(told(None), (s("/bin/owner"), Source::Proc, true));
        assert_eq!(told(Some(b"unconfined")), (s("/bin/owner"), Source::Proc, true));
        // A socket without a peer is nobody, whatever else is said.
        let p = seen_by(&Recorded { cred: None, label: Some(b"bitsos:program:1:init:1:1:/bin/owner") }, &NowOwner(Cell::new(0))).settle_by(at);
        assert_eq!((p.uid, p.untold(), p.exe), (None, false, None));
    }

    #[test]
    fn this_process_is_told_from_proc() {
        let me = std::process::id();
        let exe = std::env::current_exe().unwrap();
        assert_eq!(program(me).as_deref(), exe.to_str());
        let (a, _b) = UnixStream::pair().unwrap();
        let p = of(&a);
        assert_eq!((p.pid, p.exe.as_deref()), (me, exe.to_str()));
        // Linux keeps no such record; a kernel that does answers for the test binary too.
        assert!(matches!(p.by, Source::Proc | Source::Kernel), "{:?}", p.by);
    }
}
