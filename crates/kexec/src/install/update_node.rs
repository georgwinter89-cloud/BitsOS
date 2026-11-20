//! UPDATE: `/dev/bitsos/update`, the update service's way to the install path (root only, 0600, never a process
//! MCP's run started). The service (bitspkgd's update ops, the console's `bitsupdate`) downloads a version,
//! checks its size, SHA-256 and the desk's signature in user space, and hands the two files over here; the kernel
//! checks them again as `install_kernel` does (bitsboot's rules, the disk guard, the version marks) and installs
//! them on the medium BitsOS started from (`fs_fat::stick`: the USB stick, or the Mini-PC's `nvme_boot` partition),
//! with the same NEW/TRY/OK, confirmation and fallback as every install. No network here and no secret: the token
//! never leaves user space.
//!
//! One command per write(2), its answer (one JSON line, ending in a newline) read back with read(2) (a stream:
//! no pread, as /dev/kmsg); every open is a session:
//!
//! | command | answer |
//! |---|---|
//! | `status` | the running version and generation, the medium's generations, the pin, what starts next, the confirmation |
//! | `begin <version> <kernel_bytes> <initrd_bytes>` | `loading`; the next writes are the KERNEL's bytes, then the INITRD's |
//! | `install [unpin]` | after a whole `begin` load: the install's text, the slot, what starts next; or `refused`/`failed` |
//! | `slot <n>` | which version slot `n`'s KERNEL carries (0: a development build or a kernel from before UPDATE) |
//! | `back` | a PIN to the newest confirmed generation older than the running one (bitsboot's rule 1, next start) |
//! | `forward` | the PIN removed (the newest confirmed generation starts again) |
//! | `plan <version>` | UPD-4: free bytes, cluster size, what installing `version` frees, the sealed parts |
//! | `part <name> <hash> <files> <dirs> <bytes>` | `present`, or `writing`: the part's files follow |
//! | `file <size> <path>` | `loading` (its bytes follow, then `stored`), or `stored` for an empty file |
//! | `seal` | the part checked against its hash and kept (`parts`) |
//! | `use <name> <hash>` | the next `install` names this sealed part in the new slot's `FILES` |
//! | `confirm <n>` | VB-1: Georg's "Ja" for the running generation `n` (`confirm_ask`: the question must stand, a real press within 3 s) |
//!
//! The version `begin` names must be the one the bundle's marks carry: the service says what it verified, the
//! kernel refuses a bundle that is something else.

use super::parts::{self, Begun, Writer};
use super::parts_rules as rules;
use super::plan::{self, Gen};
use super::system::Want;
use super::{InstallError, LOCK, Opts, check, confirm, confirm_ask, install_on};
use crate::tool::Busy;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use mcp::Json;
use uapi::errno::*;
use vfs::{Cred, Errno, File};

const SEC: u64 = 1_000_000_000;
/// The longest command line: a `file` line with a whole path (PATH_MAX) and its size.
const LINE_MAX: usize = rules::PATH_MAX + 64;

/// What a `begin` announced and what arrived so far.
struct Load {
    version: u32,
    want: (usize, usize),
    kernel: Vec<u8>,
    initrd: Vec<u8>,
}

impl Load {
    fn full(&self) -> bool {
        self.kernel.len() == self.want.0 && self.initrd.len() == self.want.1
    }
    /// Takes bytes into the KERNEL, then the INITRD; returns how many it took.
    fn take(&mut self, b: &[u8]) -> usize {
        let k = (self.want.0 - self.kernel.len()).min(b.len());
        self.kernel.extend_from_slice(&b[..k]);
        let i = (self.want.1 - self.initrd.len()).min(b.len() - k);
        self.initrd.extend_from_slice(&b[k..k + i]);
        k + i
    }
}

#[derive(Default)]
struct State {
    answer: Vec<u8>,
    /// How much of `answer` was read: the node is a stream (no pread, like /dev/kmsg), each answer read once.
    read: usize,
    load: Option<Load>,
    /// UPD-4: this session's id for the parts it holds (`parts::hold`).
    id: u64,
    /// The part on its way into STORE.
    writer: Option<Writer>,
    /// The sealed parts the next `install` names in the new slot's FILES.
    uses: Vec<(String, String)>,
    /// Bytes of a file whose write failed: swallowed, so they are never read as commands.
    discard: u64,
}

struct Session {
    st: ksync::Mutex<State>,
}

/// A parsed command (pure; host-tested).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Status,
    Begin { version: u32, kernel: u64, initrd: u64 },
    Install { unpin: bool },
    Slot(u32),
    Back,
    Forward,
    Plan(u32),
    Part { name: String, hash: String, files: u64, dirs: u64, bytes: u64 },
    File { size: u64, path: String },
    Seal,
    Use { name: String, hash: String },
    Confirm(u32),
}

pub fn parse(line: &str) -> Result<Command, String> {
    let num = |s: &str| s.parse::<u64>().ok().filter(|_| s.bytes().all(|b| b.is_ascii_digit()));
    // A path may hold spaces: everything after the size is the path, byte for byte.
    if let Some(rest) = line.strip_prefix("file ") {
        let (size, path) = rest.split_once(' ').ok_or("file: a size and a path")?;
        let size = num(size).ok_or("file: the size is a byte count")?;
        return Ok(Command::File { size, path: path.to_string() });
    }
    let w: Vec<&str> = line.split_ascii_whitespace().collect();
    match w.as_slice() {
        ["plan", v] => num(v).filter(|&n| (1..=plan::NUMBER_MAX as u64).contains(&n)).map(|n| Command::Plan(n as u32)).ok_or_else(|| String::from("plan: a version number")),
        ["part", name, hash, f, d, b] => match (num(f), num(d), num(b)) {
            (Some(files), Some(dirs), Some(bytes)) => Ok(Command::Part { name: name.to_string(), hash: hash.to_string(), files, dirs, bytes }),
            _ => Err(String::from("part: <name> <hash> <files> <dirs> <bytes>")),
        },
        ["seal"] => Ok(Command::Seal),
        ["use", name, hash] => Ok(Command::Use { name: name.to_string(), hash: hash.to_string() }),
        ["status"] => Ok(Command::Status),
        ["begin", v, k, i] => {
            let version = num(v).filter(|&n| (1..=plan::NUMBER_MAX as u64).contains(&n)).ok_or("begin: the version is a number from 1 to 99999999")?;
            let (Some(kernel), Some(initrd)) = (num(k).filter(|&n| n > 0), num(i).filter(|&n| n > 0)) else {
                return Err(String::from("begin: the sizes are byte counts above 0"));
            };
            if kernel > check::KERNEL_FILE_MAX || initrd > check::INITRD_FILE_MAX {
                return Err(format!("begin: at most {} MiB of KERNEL and {} MiB of INITRD", check::KERNEL_FILE_MAX >> 20, check::INITRD_FILE_MAX >> 20));
            }
            Ok(Command::Begin { version: version as u32, kernel, initrd })
        }
        ["install"] => Ok(Command::Install { unpin: false }),
        ["install", "unpin"] => Ok(Command::Install { unpin: true }),
        ["slot", g] => num(g).filter(|&n| n <= plan::NUMBER_MAX as u64).map(|n| Command::Slot(n as u32)).ok_or_else(|| String::from("slot: a generation number")),
        ["back"] => Ok(Command::Back),
        ["forward"] => Ok(Command::Forward),
        ["confirm", g] => num(g).filter(|&n| (1..=plan::NUMBER_MAX as u64).contains(&n)).map(|n| Command::Confirm(n as u32)).ok_or_else(|| String::from("confirm: a generation number")),
        _ => Err(String::from(
            "unknown command (status, begin <version> <kernel_bytes> <initrd_bytes>, install [unpin], slot <n>, back, forward, plan <version>, part <name> <hash> <files> <dirs> <bytes>, file <size> <path>, seal, use <name> <hash>, confirm <n>)",
        )),
    }
}

fn no(kind: &str, t: &str) -> Json {
    Json::obj(alloc::vec![("ok", Json::Bool(false)), (kind, Json::str(t))])
}

/// A parts command refused: the answer, and the trace in klog (a hostile or broken update leaves one).
fn part_no(kind: &str, cmd: &str, part: &str, t: &str) -> Json {
    klog::event!(Warn, "kexec", "parts.refused", cmd = cmd, part = part, kind = kind, reason = t);
    no(kind, t)
}

fn n(v: u64) -> Json {
    Json::Num(v as f64)
}

fn gens_json(gens: &[Gen]) -> Json {
    Json::Arr(gens.iter().map(|g| Json::obj(alloc::vec![("n", n(g.n as u64)), ("markers", Json::str(&g.markers()))])).collect())
}

/// `status`: what runs, what the medium holds, what starts next, and the confirmation.
fn status() -> Json {
    let (running_gen, reason, reload) = match crate::boot::boot_info() {
        Some((bi, _)) => (bi.generation().map_or(0, |g| g.number), bi.generation().map_or("none", |g| bitsboot_pure::gens::reason_name(g.reason)), bi.reload().map_or(0, |r| r.counter)),
        None => (0, "none", 0),
    };
    let running = Json::obj(alloc::vec![
        ("version", n(bitsver::NUMBER as u64)),
        ("date", Json::str(bitsver::DATE)),
        ("generation", n(running_gen as u64)),
        ("reason", Json::str(reason)),
        ("reload", n(reload as u64)),
    ]);
    let medium = match fs_fat::stick::volume() {
        None => no("none", &format!("no boot medium mounted ({})", fs_fat::stick::last_reason().unwrap_or("none recognised"))),
        Some(vol) => {
            let writable = vol.writable();
            match super::system_stick::with_gens(10 * SEC, |s, gens, pin| {
                let next = match plan::next_boot(gens, pin) {
                    Ok((g, r, _)) => format!("{g} {r}"),
                    Err(t) => format!("none: {t}"),
                };
                let back = plan::back_target(gens, super::system_stick::running_number(s).or(Some(running_gen).filter(|&g| g > 0))).ok();
                let (cluster, _, free) = s.vol.statfs().unwrap_or((0, 0, 0));
                Ok(Json::obj(alloc::vec![
                    ("ok", Json::Bool(true)),
                    ("free", n(cluster.saturating_mul(free))),
                    ("cluster", n(cluster)),
                    ("parts", parts_json(s, gens).unwrap_or(Json::Null)),
                    ("writable", Json::Bool(writable)),
                    ("pin", pin.map_or(Json::Null, |p| n(p as u64))),
                    ("generations", gens_json(gens)),
                    ("next", Json::str(&next)),
                    ("back", back.map_or(Json::Null, |b| n(b as u64))),
                ]))
            }) {
                Ok(j) => j,
                Err(t) => no("failed", &t),
            }
        }
    };
    let c = confirm::status();
    let (held, inputs) = confirm::desktop_progress();
    let question = confirm_ask::question();
    let confirm = Json::obj(alloc::vec![
        ("pending", Json::Bool(confirm::pending().is_some())),
        ("state", Json::str(confirm_ask::state_word(&c))),
        ("asks", Json::Bool(question.is_some())),
        ("answered", Json::Bool(confirm_ask::answered() != 0)),
        ("by", Json::str(confirm_ask::by())),
        ("text", Json::str(&confirm::describe(&c))),
        ("desktop_held_s", n(held)),
        ("desktop_inputs", n(inputs as u64)),
    ]);
    Json::obj(alloc::vec![("ok", Json::Bool(true)), ("running", running), ("medium", medium), ("confirm", confirm)])
}

/// The sealed parts with the slots naming them, and the parts this boot bound.
fn parts_json(s: &fs_fat::sysapi::Sys<'_>, gens: &[Gen]) -> Result<Json, String> {
    let mut users: Vec<(u32, Vec<(String, String)>)> = Vec::new();
    for g in gens {
        users.push((g.n, parts::slot_parts(s, g.n)?));
    }
    let bound = parts::bound_folders();
    let list = parts::stored(s)?
        .into_iter()
        .map(|p| {
            let by: Vec<Json> = users.iter().filter(|(_, l)| l.iter().any(|(n, h)| *n == p.name && *h == p.hash)).map(|(g, _)| n(*g as u64)).collect();
            Json::obj(alloc::vec![
                ("name", Json::str(&p.name)),
                ("hash", Json::str(&p.hash)),
                ("files", n(p.files)),
                ("bytes", n(p.bytes)),
                ("used_by", Json::Arr(by)),
                ("bound", Json::Bool(bound.contains(&p.folder()))),
            ])
        })
        .collect();
    Ok(Json::Arr(list))
}

/// `plan <version>`: what the medium can take before anything is downloaded (bitsupdate's space check): free
/// bytes, the cluster size, what installing `version` frees (the slots it prunes or replaces, and the parts only
/// they name), what no slot uses (removed before a part would not fit), and the sealed parts.
fn plan_answer(version: u32) -> Json {
    let r = super::system_stick::with_gens(10 * SEC, |s, gens, pin| {
        let config = super::Stick::config(&super::FatStick(s)).ok().flatten();
        let keep = check::sys_keep(config.as_deref()).unwrap_or(1);
        let gone: Vec<u32> = match plan::install_versioned(gens, pin, keep, true, false, Some(version)) {
            Ok(p) => p.prune.iter().copied().chain(p.replaced).collect(),
            Err(_) => Vec::new(),
        };
        let (cluster, _, free) = s.vol.statfs().map_err(|e| format!("reading the medium's free space: {}", super::errno_name(e)))?;
        let mut freed: u64 = gens.iter().filter(|g| gone.contains(&g.n)).map(|g| g.bytes).sum();
        let mut orphans = 0u64;
        let mut users: Vec<(u32, Vec<(String, String)>)> = Vec::new();
        for g in gens {
            users.push((g.n, parts::slot_parts(s, g.n)?));
        }
        let bound = parts::bound_folders();
        for p in parts::stored(s)? {
            let by: Vec<u32> = users.iter().filter(|(_, l)| l.iter().any(|(n, h)| *n == p.name && *h == p.hash)).map(|(g, _)| *g).collect();
            if bound.contains(&p.folder()) {
                continue;
            }
            if by.is_empty() {
                orphans += p.bytes;
            } else if by.iter().all(|g| gone.contains(g)) {
                freed += p.bytes;
            }
        }
        Ok(Json::obj(alloc::vec![
            ("ok", Json::Bool(true)),
            ("free", n(cluster.saturating_mul(free))),
            ("cluster", n(cluster)),
            ("freed", n(freed)),
            ("orphans", n(orphans)),
            ("parts", parts_json(s, gens)?),
        ]))
    });
    r.unwrap_or_else(|t| no("failed", &t))
}

fn install(l: &Load, unpin: bool, uses: &[(String, String)]) -> Json {
    let refuse = |t: &str| {
        klog::event!(Warn, "kexec", "update.refused", version = l.version, reason = t; "nothing on the boot medium changed");
        no("refused", &format!("{t} (nothing on the boot medium changed)"))
    };
    let Some(vol) = fs_fat::stick::volume() else { return refuse("no boot medium mounted") };
    if !vol.writable() {
        return refuse(&format!("the boot medium is read-only ({})", fs_fat::stick::last_reason().unwrap_or("?")));
    }
    // The bundle must be the version the service verified.
    let facts = check::kernel(&l.kernel).and_then(|k| check::initrd(&l.initrd).and_then(|i| check::bundle_version(&k, &i)));
    match facts {
        Err(t) => return refuse(&t),
        Ok(v) if v != Some(l.version) => {
            return refuse(&format!("the bundle's marks say version {}, the request said {}", v.map_or(String::from("none"), |v| format!("{v}")), l.version));
        }
        Ok(_) => {}
    }
    let Some(_busy) = Busy::take() else { return refuse("a Reload, an install or the boot confirmation's write is in progress; try again in a few seconds") };
    let Some(_l) = LOCK.lock_timeout(60 * SEC) else { return no("failed", "the boot medium stayed busy for over a minute (nothing changed)") };
    klog::event!(Info, "kexec", "update.install", version = l.version, kernel_bytes = l.kernel.len(), initrd_bytes = l.initrd.len(), unpin = unpin);
    let o = Opts { unpin, ..Opts::default() };
    let mut uses = uses.to_vec();
    uses.sort();
    let r = fs_fat::stick::with_sys(|s| {
        if uses.is_empty() {
            return install_on(&super::FatStick(s), &l.kernel, &l.initrd, None, &o);
        }
        // UPD-4: the new slot names its parts; each sealed, each with its place below \BITSOS2\.
        if let Some((n, h)) = uses.iter().find(|(n, h)| parts::present(s, n, h).is_none()) {
            return Err(InstallError::Refused(format!("part {n} {} is not on the medium", &h[..16])));
        }
        parts::mount_points(s, &uses).map_err(InstallError::Refused)?;
        let files = rules::files_render(&uses);
        super::install_with(&super::FatStick(s), &l.kernel, &l.initrd, None, Some(files.as_bytes()), &o)
    });
    match r {
        Err(e) => no("refused", &format!("the boot medium went away: {}", super::errno_name(e))),
        Ok(Err(InstallError::Refused(t))) => refuse(&t),
        Ok(Err(InstallError::Failed(t))) => no("failed", &t),
        Ok(Ok(r)) => {
            if !uses.is_empty() {
                klog::event!(Info, "kexec", "parts.installed", generation = r.plan.number, parts = uses.len());
            }
            if let Some(k) = &r.kernel {
                super::system_stick::note_kernel_id(r.plan.number, k.bytes, k.fnv);
            }
            Json::obj(alloc::vec![
                ("ok", Json::Bool(true)),
                ("installed", n(r.plan.number as u64)),
                ("replaced", r.plan.replaced.map_or(Json::Null, |g| n(g as u64))),
                ("fallback", r.plan.fallback.map_or(Json::Null, |g| n(g as u64))),
                ("next", Json::str(&r.next_boot)),
                ("warnings", Json::Arr(r.warnings.iter().map(|w| Json::str(w)).collect())),
            ])
        }
    }
}

/// `slot <n>`: the version mark of generation `n`'s KERNEL (the page tells a release from a development build in
/// the slot of the same number). Only asked when a release is not above the medium's newest folder.
fn slot(want: u32) -> Json {
    let r = super::system_stick::with_gens(10 * SEC, |s, gens, _| {
        let Some(g) = gens.iter().find(|g| g.n == want) else {
            return Ok(Json::obj(alloc::vec![("ok", Json::Bool(true)), ("n", n(want as u64)), ("present", Json::Bool(false))]));
        };
        let until = time::now_ns() + 60 * SEC;
        let v = check::stream_version(
            &mut |off, b| if time::now_ns() >= until { Err(ETIMEDOUT) } else { s.read_at(want, "KERNEL", off, b) },
            1 << 20,
        )
        .map_err(|e| format!("reading generation {want}'s KERNEL: {}", super::errno_name(e)))?;
        Ok(Json::obj(alloc::vec![
            ("ok", Json::Bool(true)),
            ("n", n(want as u64)),
            ("present", Json::Bool(true)),
            ("version", n(v as u64)),
            ("markers", Json::str(&g.markers())),
        ]))
    });
    r.unwrap_or_else(|t| no("failed", &t))
}

/// `back` / `forward`: the PIN through `system_pin`'s own step (its plan, its checks, its write).
fn pin(back: bool) -> Json {
    match pin_step(back, None) {
        Ok(s) => Json::obj(alloc::vec![
            ("ok", Json::Bool(true)),
            ("pin", s.plan.pin.map_or(Json::Null, |p| n(p as u64))),
            ("next", n(s.plan.next.0.number as u64)),
            ("text", Json::str(&s.text)),
        ]),
        Err(t) => no("refused", &t),
    }
}

/// The PIN step of `back`/`forward` (also VB-1's "Zurück zur vorherigen" on the report page): the generation the
/// next start takes, or why not. `expect`: the back target the screen showed; another one now refuses.
pub fn pin_step(back: bool, expect: Option<u32>) -> Result<super::system_tool::PinStep, String> {
    let Some(_busy) = Busy::take() else { return Err(String::from("a Reload, an install or the boot confirmation's write is in progress; try again in a few seconds")) };
    let want = if back {
        let target = super::system_stick::with_gens(10 * SEC, |s, gens, _| {
            let running = super::system_stick::running_number(s).or_else(|| crate::boot::boot_info().and_then(|(bi, _)| bi.generation()).map(|g| g.number));
            Ok(plan::back_target(gens, running))
        });
        match target {
            Ok(Ok(g)) => Want::Pin(expected(g, expect)?),
            Ok(Err(t)) | Err(t) => return Err(t),
        }
    } else {
        Want::Off
    };
    super::system_tool::pin_step(want, false, false, 60 * SEC).map_err(|e| match e {
        mcp::ToolError::BadArgument(t) | mcp::ToolError::Refused(t) | mcp::ToolError::Failed(t) => t,
    })
}

/// The back target `g` against the one a screen showed (pure).
pub fn expected(g: u32, expect: Option<u32>) -> Result<u32, String> {
    match expect {
        Some(e) if e != g => Err(format!("the newest confirmed version below is now {g}, not the {e} the screen showed; nothing changed")),
        _ => Ok(g),
    }
}

/// `confirm <n>`: Georg's "Ja" handed to the watch; the write's outcome is in `status` (state, by).
fn answer(g: u32) -> Json {
    match confirm_ask::answer(g, false) {
        Ok(()) => Json::obj(alloc::vec![("ok", Json::Bool(true)), ("confirming", n(g as u64))]),
        Err(t) => no("refused", &t),
    }
}

impl Session {
    fn command(&self, st: &mut State, line: &str) -> Json {
        let c = match parse(line) {
            Ok(c) => c,
            Err(t) => return no("bad_request", &t),
        };
        if !matches!(c, Command::Install { .. }) {
            st.load = None;
        }
        // A part's file announcements belong to its writer; anything else but the parts' commands ends the parts'
        // plan of this session.
        if !matches!(c, Command::File { .. } | Command::Seal | Command::Part { .. } | Command::Use { .. } | Command::Begin { .. } | Command::Install { .. }) {
            st.uses.clear();
            self.drop_writer(st);
        }
        match c {
            Command::Status => status(),
            Command::Begin { version, kernel, initrd } => {
                // Both files and the page cache's copy while they are written: a quarter of what can be spared.
                if kernel + initrd > crate::stage::budget() / 4 {
                    return no("refused", &format!("{} MiB do not fit in memory now", (kernel + initrd) >> 20));
                }
                let (mut k, mut i) = (Vec::new(), Vec::new());
                if k.try_reserve_exact(kernel as usize).is_err() || i.try_reserve_exact(initrd as usize).is_err() {
                    return no("refused", "out of memory for the bundle");
                }
                st.load = Some(Load { version, want: (kernel as usize, initrd as usize), kernel: k, initrd: i });
                Json::obj(alloc::vec![("ok", Json::Bool(true)), ("loading", n(kernel + initrd))])
            }
            Command::Install { unpin } => match st.load.take() {
                Some(l) if l.full() => {
                    let uses = core::mem::take(&mut st.uses);
                    install(&l, unpin, &uses)
                }
                Some(_) => no("bad_request", "install before the whole bundle arrived"),
                None => no("bad_request", "install without begin"),
            },
            Command::Slot(g) => slot(g),
            Command::Back => pin(true),
            Command::Forward => pin(false),
            Command::Plan(v) => plan_answer(v),
            Command::Part { name, hash, files, dirs, bytes } => {
                if st.writer.is_some() {
                    self.drop_writer(st);
                    return part_no("bad_request", "part", &name, "part before the last part was sealed (it was given up)");
                }
                let keep: Vec<String> = st.uses.iter().map(|(n, h)| rules::folder(n, h)).collect();
                match fs_fat::stick::with_sys(|s| Writer::begin(s, st.id, &keep, &name, &hash, files, dirs, bytes)) {
                    Err(e) => part_no("refused", "part", &name, &format!("no boot medium: {}", super::errno_name(e))),
                    Ok(Err(t)) => part_no("refused", "part", &name, &t),
                    Ok(Ok(Begun::Present(p))) => Json::obj(alloc::vec![("ok", Json::Bool(true)), ("present", Json::Bool(true)), ("files", n(p.files)), ("bytes", n(p.bytes))]),
                    Ok(Ok(Begun::Writing(w))) => {
                        st.writer = Some(w);
                        Json::obj(alloc::vec![("ok", Json::Bool(true)), ("writing", Json::Bool(true))])
                    }
                }
            }
            Command::File { size, path } => {
                let Some(w) = st.writer.as_mut() else { return part_no("bad_request", "file", "", "file without part") };
                let part = w.name.clone();
                match fs_fat::stick::with_sys(|s| w.file(s, size, &path)) {
                    Ok(Ok(true)) => Json::obj(alloc::vec![("ok", Json::Bool(true)), ("loading", n(size))]),
                    Ok(Ok(false)) => Json::obj(alloc::vec![("ok", Json::Bool(true)), ("stored", n(size))]),
                    Ok(Err(t)) => {
                        self.drop_writer(st);
                        part_no("refused", "file", &part, &t)
                    }
                    Err(e) => {
                        self.drop_writer(st);
                        part_no("refused", "file", &part, &format!("no boot medium: {}", super::errno_name(e)))
                    }
                }
            }
            Command::Seal => {
                let Some(w) = st.writer.take() else { return part_no("bad_request", "seal", "", "seal without part") };
                let part = w.name.clone();
                match fs_fat::stick::with_sys(|s| w.seal(s)) {
                    Ok(Ok(p)) => {
                        parts::hold(st.id, &p.folder());
                        Json::obj(alloc::vec![("ok", Json::Bool(true)), ("sealed", Json::str(&p.name)), ("files", n(p.files)), ("bytes", n(p.bytes))])
                    }
                    Ok(Err(t)) => part_no("refused", "seal", &part, &t),
                    Err(e) => part_no("refused", "seal", &part, &format!("no boot medium: {}", super::errno_name(e))),
                }
            }
            Command::Use { name, hash } => {
                if rules::name_ok(&name).is_err() || !rules::hex_ok(&hash) {
                    return part_no("bad_request", "use", "", "use: a part's name and its 64 hex digits");
                }
                match fs_fat::stick::with_sys(|s| parts::present(s, &name, &hash)) {
                    Ok(Some(_)) if !st.uses.iter().any(|(n, _)| *n == name) => {
                        parts::hold(st.id, &rules::folder(&name, &hash));
                        klog::event!(Info, "kexec", "parts.used", part = name.as_str(), hash = &hash[..16]);
                        st.uses.push((name, hash));
                        Json::obj(alloc::vec![("ok", Json::Bool(true)), ("uses", n(st.uses.len() as u64))])
                    }
                    Ok(Some(_)) => part_no("bad_request", "use", &name, &format!("use: part {name} named twice")),
                    Ok(None) => part_no("refused", "use", &name, &format!("part {name} {} is not on the medium", &hash[..16])),
                    Err(e) => part_no("refused", "use", &name, &format!("no boot medium: {}", super::errno_name(e))),
                }
            }
            Command::Confirm(g) => answer(g),
        }
    }

    /// Gives up the part being written (its `.NEW` folder goes).
    fn drop_writer(&self, st: &mut State) {
        if let Some(w) = st.writer.take() {
            let _ = fs_fat::stick::with_sys(|s| w.abort(s));
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let st = self.st.get_mut();
        if let Some(w) = st.writer.take() {
            let _ = fs_fat::stick::with_sys(|s| w.abort(s));
        }
        parts::release(st.id);
    }
}

impl Session {
    fn read(&self, buf: &mut [u8]) -> usize {
        let mut st = self.st.lock();
        let from = st.read.min(st.answer.len());
        let k = (st.answer.len() - from).min(buf.len());
        buf[..k].copy_from_slice(&st.answer[from..from + k]);
        st.read = from + k;
        k
    }
}

impl File for Session {
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        Ok(self.read(buf))
    }
    fn read_stream(&self, buf: &mut [u8], _nonblock: bool) -> Result<usize, Errno> {
        Ok(self.read(buf))
    }
    fn write_at(&self, _pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        let mut st = self.st.lock();
        if let Some(l) = st.load.as_mut().filter(|l| !l.full()) {
            let took = l.take(buf);
            if l.full() {
                st.answer = Json::obj(alloc::vec![("ok", Json::Bool(true)), ("loaded", n((l.want.0 + l.want.1) as u64))]).to_string().into_bytes();
                st.answer.push(b'\n');
                st.read = 0;
            }
            return Ok(took);
        }
        if st.discard > 0 {
            let k = (st.discard as usize).min(buf.len());
            st.discard -= k as u64;
            return Ok(k);
        }
        let want = st.writer.as_ref().map_or(0, Writer::expecting);
        if want > 0 {
            let r = fs_fat::stick::with_sys(|s| st.writer.as_mut().map(|w| w.take(s, buf)));
            let a = match r {
                Ok(Some(Ok((k, false)))) => return Ok(k),
                Ok(Some(Ok((k, true)))) => {
                    let a = Json::obj(alloc::vec![("ok", Json::Bool(true)), ("stored", n(want))]);
                    (a, k)
                }
                other => {
                    let t = match other {
                        Ok(Some(Err(t))) => t,
                        Err(e) => format!("no boot medium: {}", super::errno_name(e)),
                        _ => String::from("no file being written"),
                    };
                    let part = st.writer.as_ref().map(|w| w.name.clone()).unwrap_or_default();
                    klog::event!(Warn, "kexec", "parts.refused", cmd = "bytes", part = part.as_str(), kind = "refused", reason = t.as_str());
                    self.drop_writer(&mut st);
                    let k = (want as usize).min(buf.len());
                    st.discard = want - k as u64;
                    (no("refused", &t), k)
                }
            };
            st.answer = a.0.to_string().into_bytes();
            st.answer.push(b'\n');
            st.read = 0;
            return Ok(a.1);
        }
        if buf.len() > LINE_MAX {
            return Err(EINVAL);
        }
        let raw = core::str::from_utf8(buf).map_err(|_| EINVAL)?;
        let raw = raw.strip_suffix('\n').unwrap_or(raw);
        // A file's path is taken byte for byte; every other command may carry spaces around it.
        let line = if raw.starts_with("file ") { raw } else { raw.trim() };
        let mut a = self.command(&mut st, line).to_string().into_bytes();
        a.push(b'\n');
        st.answer = a;
        st.read = 0;
        Ok(buf.len())
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o600
    }
    fn rdev(&self) -> u64 {
        rdev_number()
    }
}

/// A misc device number from Linux's dynamic range, below the journal's 240 and FS's 239.
fn rdev_number() -> u64 {
    vfs::abi::encode_dev(10, 238)
}

struct Node;

/// Opens refused (not root, or MCP's run): the ktest's witness.
pub static OPEN_REFUSED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

impl File for Node {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o600
    }
    fn rdev(&self) -> u64 {
        rdev_number()
    }
    fn open_node(&self, _flags: u64, who: Cred) -> Option<Result<Arc<dyn File>, Errno>> {
        // 0600 root:root; refused here too, whatever its mode was changed to. Never a process MCP's run started
        // (inherited over fork and exec), as /dev/bitsos/disksetup: over MCP the desk installs with install_kernel
        // and install_system, whose own checks and log stay the one remote way into SYS.
        let mcp = proc::current().is_some_and(|p| p.program == proc::MCP_PROGRAM);
        if who.uid != 0 || mcp {
            OPEN_REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            klog::event!(Warn, "kexec", "update.open_refused", uid = who.uid, mcp = mcp; "the update node is for root at the machine (bitspkgd, bitsupdate), never over MCP");
            return Some(Err(if mcp { EPERM } else { EACCES }));
        }
        let st = State { id: parts::session_id(), ..State::default() };
        Some(Ok(Arc::new(Session { st: ksync::Mutex::named("kexec.update", st) })))
    }
}

fn make() -> Arc<dyn File> {
    Arc::new(Node)
}
vfs::devnode!("/dev/bitsos/update", make);

/// `/proc/bitsos/version`: this build's version for user space (`number=0`: a development build).
fn version_render() -> String {
    format!("number={} date={}\n", bitsver::NUMBER, bitsver::DATE)
}
vfs::procfile!("/proc/bitsos/version", version_render);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zurueck_goes_only_where_the_screen_said() {
        assert_eq!(super::expected(167, Some(167)), Ok(167));
        assert_eq!(super::expected(167, None), Ok(167), "the node's own back names no target");
        assert!(super::expected(166, Some(167)).unwrap_err().contains("not the 167"));
    }

    #[test]
    fn commands() {
        assert_eq!(parse("status"), Ok(Command::Status));
        assert_eq!(parse(" begin 165 1000 2000 "), Ok(Command::Begin { version: 165, kernel: 1000, initrd: 2000 }));
        assert_eq!(parse("install"), Ok(Command::Install { unpin: false }));
        assert_eq!(parse("install unpin"), Ok(Command::Install { unpin: true }));
        assert_eq!(parse("back"), Ok(Command::Back));
        assert_eq!(parse("forward"), Ok(Command::Forward));
        assert_eq!(parse("slot 165"), Ok(Command::Slot(165)));
        assert_eq!(parse("confirm 168"), Ok(Command::Confirm(168)));
        for bad in ["confirm", "confirm 0", "confirm x", "confirm 1 2", "confirm -3", "begin 0 1 1", "begin 165 0 1", "begin 165 1", "begin +5 1 1", "begin 100000000 1 1", "install now", "rm -rf", "slot", "slot x", "slot 1 2"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert!(parse(&format!("begin 1 {} 1", check::KERNEL_FILE_MAX + 1)).is_err());
    }

    #[test]
    fn a_load_takes_the_kernel_then_the_initrd() {
        let mut l = Load { version: 1, want: (3, 2), kernel: Vec::new(), initrd: Vec::new() };
        assert_eq!(l.take(b"ab"), 2);
        assert_eq!(l.take(b"cdefg"), 3);
        assert!(l.full());
        assert_eq!((l.kernel.as_slice(), l.initrd.as_slice()), (&b"abc"[..], &b"de"[..]));
        assert_eq!(l.take(b"x"), 0);
    }
}
