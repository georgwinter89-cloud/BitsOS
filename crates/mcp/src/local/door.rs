//! Who may use the local tools socket. The answer never comes from the peer's own bytes: the socket gives the
//! peer's pid and uid (SO_PEERCRED) and the program image that called connect (`By`, both taken by the kernel
//! at connect), the process layer the process that has this pid now, and the door compares that process's
//! executable (Linux `mm->exe_file`) with the file the kernel's own root view has at the program's path
//! (`gate::identity::Measured`: the same inode; a running program's file cannot be written, ETXTBSY). A name,
//! an argv[0], a comm or a copy of the file prove nothing.
//!
//! The process that has the pid now must be the very image that called connect (INT-CL3T): a program that
//! connects, hands the socket to a helper and then becomes the door's program (execve) is another image than
//! the one on record, and so is whoever got the pid after an exit. Where a link keeps no record (host tests'
//! std sockets, `By::Untold`) the pid alone is looked up, as before.
//!
//! The image must live where the system's programs live (`Proc::native`): whoever made a user namespace
//! reads and writes what runs in it however that closed its memory, and whoever made a mount namespace chose
//! the files the program finds under its fixed paths; either way somebody else would speak through it.
//!
//! The file is looked up when the door is made and again whenever a peer's executable is not the one known
//! (the program was not there yet, or root put a newer one at the path): only root can change what the root
//! view has there, and root is not whom this door keeps out. A process of the older file is then no longer
//! admitted.
//!
//! The image must also be closed to its own user's other programs (not dumpable): otherwise any of them could
//! write into the real program (process_vm_writev, /proc/<pid>/mem) and speak through it. The process layer
//! closes the door's program at execve (`protects`); an image that is still open is refused.
//!
//! The process object is kept with the connection and compared before every request: BitsOS makes one
//! `Process` per program image, so the same pid after an exit (pid reuse) or after an execve is another
//! object and the connection ends there.
//!
//! What this does not prove: the record names the program that
//! called connect, not who drives it (its terminal, its arguments). So the tools behind this door are only
//! those a program of the same user may read anyway (`ToolEntry::local`).

use super::Peer;
use crate::gate::identity::Measured;
use crate::svc::By;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use core::any::Any;
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::SpinLock;

/// The program admitted in MC-L: the assistant.
pub const PROGRAM: &str = "/bin/assist";

/// Decides at accept who is at the other end.
pub trait Door: Send + Sync {
    /// `Err` is the reason for the log and the refusal line.
    fn admit(&self, peer: Peer, by: By) -> Result<Box<dyn Pass>, String>;
}

/// An admitted peer.
pub trait Pass: Send + Sync {
    /// Before every request: still the very program image that was admitted.
    fn still(&self) -> Result<(), String>;
    /// The caller for B3 and the log.
    fn subject(&self) -> policy::Subject;
}

/// A process as the process layer reports it (mcp sits below proc).
pub struct Proc {
    /// Program, pid and effective uid.
    pub subject: policy::Subject,
    /// The image's executable (None for an image started from memory).
    pub exe: Option<vfs::Path>,
    /// The program image itself (proc's `Process`): only its address is compared, and holding it keeps that
    /// address from being given to a later process.
    pub image: Arc<dyn Any + Send + Sync>,
    pub finished: bool,
    /// Its credentials are in the initial user namespace and its files are the boot view's.
    pub native: bool,
    /// Other programs of the same user may read and write its memory (Linux SUID_DUMP_USER, what every
    /// execve gives unless the process layer closes the image).
    pub dumpable: bool,
}

/// The process that has `pid` now.
pub type Lookup = fn(u32) -> Option<Proc>;

static LOOKUP: ksync::Once<Lookup> = ksync::Once::new();

/// Installed once by the process layer (crates/proc/src/mcp_run/local.rs).
pub fn set_lookup(f: Lookup) {
    LOOKUP.get_or_init(|| f);
}

fn no_lookup(_pid: u32) -> Option<Proc> {
    None
}

fn same(a: &Arc<dyn Any + Send + Sync>, b: &Arc<dyn Any + Send + Sync>) -> bool {
    core::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b))
}

struct Program {
    path: String,
    /// The file the root view had at `path` when last looked (None: none yet).
    measured: SpinLock<Option<Arc<Measured>>>,
    /// `local.no_program` was said (once per door, not per peer).
    said_missing: AtomicBool,
    /// None: the installed hook, looked at when asked (the door is made before the process layer starts).
    lookup: Option<Lookup>,
}

impl Program {
    fn new(path: &str, lookup: Option<Lookup>) -> Program {
        let p = Program { path: String::from(path), measured: SpinLock::named("mcp.local.measured", None), said_missing: AtomicBool::new(false), lookup };
        let _ = p.measure();
        p
    }

    /// Looks the path up again and keeps what is there.
    fn measure(&self) -> Option<Arc<Measured>> {
        // Outside the lock: the lookup takes VFS locks and may sleep.
        let fresh = Measured::of(&self.path).map(Arc::new);
        match &fresh {
            Some(_) => {
                if self.said_missing.swap(false, Ordering::AcqRel) {
                    klog::event!(Info, "mcp", "local.program_found", program = self.path.as_str(); "the local tools socket's program is there now");
                }
            }
            None => {
                if !self.said_missing.swap(true, Ordering::AcqRel) {
                    klog::event!(Warn, "mcp", "local.no_program", program = self.path.as_str(); "the local tools socket admits nobody: the program is not in the root view's files (asked again at every connect)");
                }
            }
        }
        *self.measured.lock() = fresh.clone();
        fresh
    }

    /// Whether `exe` is the file at the door's path. The known file answers without a lookup; anything else
    /// asks the path again (a program that came after the door, or a newer one root put there).
    fn is_program(&self, exe: &vfs::Path) -> bool {
        let known = self.measured.lock().clone();
        if known.is_some_and(|m| m.is(exe)) {
            return true;
        }
        self.measure().is_some_and(|m| m.is(exe))
    }

    fn judge(&self, peer: Peer, by: &By) -> Result<Proc, String> {
        if peer.pid == 0 {
            return Err(String::from("not a program (pid 0)"));
        }
        let lookup = self.lookup.or_else(|| LOOKUP.get().copied()).unwrap_or(no_lookup);
        let p = lookup(peer.pid).filter(|p| !p.finished).ok_or_else(|| format!("pid {} does not run (any more)", peer.pid))?;
        match by {
            By::Untold => {}
            By::Image(Some(at_connect)) if same(at_connect, &p.image) => {}
            By::Image(_) => return Err(format!("pid {} is not the program that connected (that one ended or became another program)", peer.pid)),
        }
        if p.subject.uid != peer.uid {
            return Err(format!("pid {} runs as uid {} now, it connected as uid {}", peer.pid, p.subject.uid, peer.uid));
        }
        let exe = p.exe.as_ref().ok_or_else(|| format!("pid {} has no executable file", peer.pid))?;
        if !self.is_program(exe) {
            if self.measured.lock().is_none() {
                return Err(format!("there is no {}", self.path));
            }
            return Err(format!("pid {} does not run the boot's {}", peer.pid, self.path));
        }
        if !p.native {
            return Err(format!("pid {} runs {} in a user or mount namespace of its own", peer.pid, self.path));
        }
        if p.dumpable {
            return Err(format!("pid {} runs {}, but its memory is open to other programs of its user", peer.pid, self.path));
        }
        Ok(p)
    }
}

static KERNEL_PROGRAM: ksync::Once<Arc<Program>> = ksync::Once::new();

fn kernel_program() -> &'static Arc<Program> {
    KERNEL_PROGRAM.get_or_init(|| Arc::new(Program::new(PROGRAM, None)))
}

/// Whether `exe` is the program of the kernel's local door. The process layer asks at execve and spawn and
/// then makes the new image not dumpable, as it does for set-id programs (`proc::mcp_run::local::
/// close_door_program`, INT-CL3T): the door admits only an image closed like that.
pub fn protects(exe: &vfs::Path) -> bool {
    kernel_program().is_program(exe)
}

/// The door for one program.
pub struct ProgramDoor(Arc<Program>);

impl ProgramDoor {
    /// The kernel's door, for `PROGRAM` (the one `protects` answers for).
    pub fn kernel() -> ProgramDoor {
        ProgramDoor(kernel_program().clone())
    }
    /// A door for `path`, looked up now (ktests).
    pub fn measure(path: &str) -> ProgramDoor {
        ProgramDoor::with(path, None)
    }
    /// With a lookup of the caller's own (ktests).
    pub fn with(path: &str, lookup: Option<Lookup>) -> ProgramDoor {
        ProgramDoor(Arc::new(Program::new(path, lookup)))
    }
    /// The judgement alone (ktests): the process behind `peer` when it may pass.
    pub fn judge(&self, peer: Peer, by: &By) -> Result<Proc, String> {
        self.0.judge(peer, by)
    }
}

struct ProgramPass {
    program: Arc<Program>,
    peer: Peer,
    by: By,
    proc: Proc,
}

impl Door for ProgramDoor {
    fn admit(&self, peer: Peer, by: By) -> Result<Box<dyn Pass>, String> {
        let proc = self.0.judge(peer, &by)?;
        Ok(Box::new(ProgramPass { program: self.0.clone(), peer, by, proc }))
    }
}

impl Pass for ProgramPass {
    fn still(&self) -> Result<(), String> {
        let now = self.program.judge(self.peer, &self.by)?;
        if !same(&now.image, &self.proc.image) {
            return Err(format!("pid {} is another process or another program image than the one admitted", self.peer.pid));
        }
        Ok(())
    }
    fn subject(&self) -> policy::Subject {
        self.proc.subject
    }
}
