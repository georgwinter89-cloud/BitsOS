//! `/dev/bitsos/disksetup`, the kernel's side of `bitsdisk` (NV-2p, Georg 5.11.): the keyboard-only path that gives
//! BitsOS one partition of a disk. One command per write, the answer read back (first word `ok` or `refused`).
//!
//! - `list`: every namespace with model, serial, size and partitions, and what each partition may become.
//! - `prepare <disk> <number>`: everything is checked once; from here on the kernel watches the input sink. Answer
//!   `ok type <code>`, or `ok remake <code>` when a half-made pool would be erased.
//! - `confirm`: the line typed since `prepare` is read out of the sink itself (`typed::judge`): real key presses
//!   only, the serial's last six characters, an Enter at most `FRESH_NS` old. Then the window is armed for this boot,
//!   the `nvme_data=` line goes into CONFIG.TXT (`fs_fat::protect::with_keyboard_line`) and the pool and ext4 are
//!   made (`fs_ext4::data::format_pool`). The window closes again; the next boot checks everything itself.
//! - `cancel`.
//!
//! One window at a time: a boot that has armed one (/data lies on it) refuses every other partition (`window_busy`),
//! a controller and a disk have room for one range only, and CONFIG.TXT for one key.
//!
//! Who can reach this: root, never a process MCP's `run` started (`proc::MCP_PROGRAM`, inherited over fork and exec);
//! the file tools do not write below /dev. And whoever reaches it still needs a person at the machine: nothing a
//! program sends counts as typed, what MCP or a program types is marked injected and refuses the confirmation.
//! `nvme_write=on` is not touched here: it stays Georg's own edit, and without it `prepare` says so.

use crate::cmd::ONCS_DSM;
use crate::typed::{self, Refused};
use crate::window as drv;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use block::gpt::window::{self, Boot, Key, SIG_BITSVOL, Why};
use block::table::{GUID_LINUX_DATA, GUID_MS_BASIC_DATA, GptEntry, guid_text};
use block::{Disk, DiskDevice};
use core::sync::atomic::{AtomicBool, Ordering};
use fs_ext4::data::PoolLook;
use console::evdev::{InputEvent, sink};
use ksync::SpinLock;
use uapi::errno::*;
use vfs::Errno;
use vfs::node::File;
use vfs::pagecache::SyncKind;

const CONFIG_TXT: &str = "/boot/BITSOS2/CONFIG.TXT";
/// How old the confirming Enter may be when `confirm` arrives (bitsdisk sends it right after reading the line).
const FRESH_NS: u64 = 60_000_000_000;
/// A character device of the misc major, next to MC-8a's gate.
const DEV: (u32, u32) = (10, 237);
const LINE_MAX: usize = 256;

/// The pending `prepare`: at most one on the machine.
#[derive(Clone)]
struct Pending {
    disk: String,
    number: u32,
    key: Key,
    /// The first input event that belongs to the typed line.
    seq: u64,
    /// The partition holds a half-made pool of the key on the stick: it is made again.
    remake: bool,
}

static PENDING: SpinLock<Option<Pending>> = SpinLock::named("nvme.setup.pending", None);
/// A `confirm` holds the window open: a second one must not arm over it, nor its end close the other's.
static MAKING: AtomicBool = AtomicBool::new(false);

fn refused(word: &str, text: &str) -> String {
    format!("refused {word} {text}\n")
}

fn why(w: &Why) -> String {
    refused(w.word(), &w.text())
}

/// What CONFIG.TXT on the stick says now (the boot's copy may be older: an earlier run of this very tool).
fn stick_key() -> Option<String> {
    let text = vfs::root_ns().read_file(CONFIG_TXT, fs_fat::protect::CONFIG_MAX as usize).ok()?;
    let text = String::from_utf8_lossy(&text);
    let mut v = None;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some((k, val)) = line.split_once('=')
            && k.trim() == window::KEY
        {
            v = Some(String::from(val.trim()));
        }
    }
    v
}

/// The namespace disks: (disk name, its controller, nsid).
fn namespaces() -> Vec<(Arc<Disk>, Arc<crate::Nvme>, u32)> {
    let mut out = Vec::new();
    for c in crate::controllers() {
        for ns in c.info().namespaces.iter().filter(|n| n.unusable.is_none()) {
            if let Some(d) = block::find(&format!("{}n{}", c.name, ns.nsid)) {
                out.push((d, c.clone(), ns.nsid));
            }
        }
    }
    out
}

/// What may become of a partition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Offer {
    /// Nothing on it: pool and ext4 are made after the typed code.
    New,
    /// The pool the stick's key names.
    ThisPool,
}

pub(crate) fn key_of(serial: &str, e: &GptEntry) -> Key {
    Key { serial: String::from(serial.trim()), guid: e.unique, first: e.first, blocks: e.last.saturating_sub(e.first).saturating_add(1) }
}

pub(crate) fn offer(serial: &str, d: &Arc<Disk>, e: &GptEntry, stick: Option<&str>) -> Result<Offer, Why> {
    let key = key_of(serial, e);
    let (scan, total) = (d.scan(), d.dev_info().blocks);
    window::check_table(&key.guid, total, &scan, &key, window::esp_key()?.as_ref())?;
    if key.render().is_none() {
        return Err(Why::BadKey("the serial cannot stand in a CONFIG line"));
    }
    let sig = drv::read_signature(d, key.first, key.blocks).map_err(|er| Why::Unclear(format!("the partition's start cannot be read: {}", er.as_str())))?;
    match sig {
        None => Ok(Offer::New),
        Some(SIG_BITSVOL) if stick.and_then(|v| Key::parse(v).ok()).is_some_and(|k| k == key) => Ok(Offer::ThisPool),
        Some(s) => Err(Why::Signature(s)),
    }
}

/// Why no window can be opened for a setup now. Arming would overwrite the one range the disk and the controller
/// hold, and the end of the setup would close it: /data would lose its writes until the next boot.
pub(crate) fn busy(boot: &Boot, disk_window: bool, ctrl_window: bool) -> Option<String> {
    if let Boot::Armed { part, .. } = boot {
        return Some(format!("/data lies on {part}: take the nvme_data= line out of CONFIG.TXT and restart first"));
    }
    (disk_window || ctrl_window).then(|| String::from("a write window is open on this disk"))
}

fn busy_now(d: &Disk, c: &crate::Nvme) -> Option<String> {
    busy(&window::boot_state(), d.window().is_some(), c.window().is_some())
}

/// What `confirm` is going to do with a partition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    /// Nothing to do: the pool is there.
    Already,
    /// Pool and ext4 are made; `remake`: over a half-made pool.
    Make { remake: bool },
}

pub(crate) fn plan_of(o: Offer, look: impl FnOnce() -> PoolLook) -> Result<Plan, String> {
    match o {
        Offer::New => Ok(Plan::Make { remake: false }),
        Offer::ThisPool => match look() {
            PoolLook::Data => Ok(Plan::Already),
            PoolLook::HalfMade => Ok(Plan::Make { remake: true }),
            PoolLook::Unclear(e) => Err(refused("unclear", &format!("the pool on this partition cannot be judged ({e}): nothing is erased"))),
        },
    }
}

fn plan_on(disk: &str, number: u32, o: Offer) -> Result<Plan, String> {
    plan_of(o, || match block::find(&block::gpt::part_name(disk, number)) {
        Some(part) => fs_ext4::data::look_at_pool(Arc::new(DiskDevice(part))),
        None => PoolLook::Unclear(String::from("the partition is not registered")),
    })
}

fn clean(s: &str) -> String {
    s.chars().map(|c| if c == '\t' || c == '\n' { ' ' } else { c }).collect()
}

fn list() -> String {
    let stick = stick_key();
    let mut out = String::from("ok\n");
    out.push_str(&format!("nvme_write\t{}\n", if window::write_key_on() { "on" } else { "off" }));
    out.push_str(&format!("key\t{}\n", stick.as_deref().unwrap_or("-")));
    for (d, c, _) in namespaces() {
        let info = c.info();
        let id = info.ident.unwrap_or_default();
        let scan = d.scan();
        let (bs, blocks) = (d.block_size(), d.dev_info().blocks);
        out.push_str(&format!("disk\t{}\t{}\t{}\t{}\t{}\t{}\n", d.name(), clean(&id.model), clean(&id.serial), blocks, bs, id.oncs & ONCS_DSM != 0));
        let Some(g) = scan.gpt.as_ref() else { continue };
        let occupied = busy_now(&d, &c);
        for e in &g.entries {
            let sig = drv::read_signature(&d, e.first, e.last.saturating_sub(e.first).saturating_add(1));
            let sig = match sig {
                Ok(None) => String::from("-"),
                Ok(Some(s)) => String::from(s),
                Err(_) => String::from("unreadable"),
            };
            let kind = match e.kind {
                GUID_MS_BASIC_DATA => "Microsoft basic data".to_string(),
                GUID_LINUX_DATA => "Linux data".to_string(),
                k => guid_text(&k),
            };
            let verdict = match offer(&id.serial, &d, e, stick.as_deref()) {
                Ok(Offer::New) => occupied.as_ref().map_or_else(|| String::from("free"), |t| format!("window_busy: {t}")),
                Ok(Offer::ThisPool) => String::from("this_pool"),
                Err(w) => format!("{}: {}", w.word(), w.text()),
            };
            out.push_str(&format!("part\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n", e.number, e.first, e.last.saturating_sub(e.first).saturating_add(1), clean(&kind), guid_text(&e.unique), clean(&sig), clean(&verdict)));
        }
    }
    out
}

/// The next event's number: what comes after it was typed after this moment.
fn sink_end() -> u64 {
    sink().recent_since(0).last().map_or(0, |(s, _)| s + 1)
}

fn prepare(disk: &str, number: u32) -> String {
    if !window::write_key_on() {
        return why(&Why::NoWriteKey);
    }
    let Some((d, c, _)) = namespaces().into_iter().find(|(d, ..)| d.name() == disk) else { return refused("no_disk", "no such disk") };
    if let Boot::Armed { part, .. } = window::boot_state()
        && part == block::gpt::part_name(disk, number)
    {
        return String::from("ok already /data lies on this partition\n");
    }
    if let Some(t) = busy_now(&d, &c) {
        return refused("window_busy", &t);
    }
    let serial = drv::serial_of(&*c);
    let scan = d.scan();
    let Some(e) = scan.gpt.as_ref().and_then(|g| g.entries.iter().find(|e| e.number == number)) else { return refused("no_partition", "no such partition") };
    let stick = stick_key();
    let o = match offer(&serial, &d, e, stick.as_deref()) {
        Ok(o) => o,
        Err(w) => return why(&w),
    };
    let key = key_of(&serial, e);
    let remake = match plan_on(disk, number, o) {
        Ok(Plan::Already) => return String::from("ok already the pool is there; a restart mounts it at /data\n"),
        Ok(Plan::Make { remake }) => remake,
        Err(answer) => return answer,
    };
    let code = key.code();
    *PENDING.lock() = Some(Pending { disk: String::from(disk), number, key, seq: sink_end(), remake });
    klog::event!(Info, "nvme", "setup.prepared", disk = disk, number = number, remake = remake; "bitsdisk: waiting for the serial's last characters typed at the machine");
    format!("ok {} {code}\n", if remake { "remake" } else { "type" })
}

/// Judges the look-back since `seq` for the typed `code`.
fn proof(seq: u64, code: &str) -> Result<(), Refused> {
    let seen = sink().recent_since(seq);
    // The sink numbers every event: a first one above `seq` means the look-back lost the line's start.
    let complete = seen.first().is_none_or(|(s, _)| *s == seq);
    let events: Vec<InputEvent> = seen.into_iter().map(|(_, e)| e).collect();
    typed::judge(&events, complete, code, time::now_ns(), FRESH_NS)
}

fn write_key(value: &str) -> Result<(), Errno> {
    let ns = vfs::root_ns();
    let line = format!("\n{}={value}\n", window::KEY);
    fs_fat::protect::with_keyboard_line(window::KEY, value, || {
        let f = vfs::Ctx::kernel(&ns).open(None, CONFIG_TXT.as_bytes(), vfs::abi::O_WRONLY | vfs::abi::O_APPEND, 0)?;
        f.write(line.as_bytes())?;
        // Inside the permit: the write-back asks the guard again (protect::config_guard).
        ns.lookup_path(CONFIG_TXT.as_bytes(), true)?.dentry.inode.sync(SyncKind::Fsync)
    })
}

fn confirm() -> String {
    let Some(p) = PENDING.lock().take() else { return refused("no_prepare", "nothing was prepared") };
    if let Err(e) = proof(p.seq, &p.key.code()) {
        klog::event!(Warn, "nvme", "setup.refused", disk = p.disk.as_str(), why = e.word(); "bitsdisk: the confirmation did not come from the machine's own keyboard, or was wrong: nothing written");
        return refused(e.word(), "the typed code does not count: nothing was written");
    }
    let Some((d, c, nsid)) = namespaces().into_iter().find(|(d, ..)| d.name() == p.disk) else { return refused("no_disk", "the disk went away") };
    let Some(part) = block::find(&block::gpt::part_name(&p.disk, p.number)) else { return refused("no_partition", "the partition is not registered") };
    // Everything again, on the table as it is now: nothing may have changed since `prepare`.
    d.rescan();
    if let Some(t) = busy_now(&d, &c) {
        return refused("window_busy", &t);
    }
    let w = match drv::check(&*c, &d, &p.key) {
        Ok(w) => w,
        Err(w) => return why(&w),
    };
    let scan = d.scan();
    let Some(e) = scan.gpt.as_ref().and_then(|g| g.entries.iter().find(|e| e.number == p.number)) else { return refused("no_partition", "no such partition") };
    let stick = stick_key();
    let o = match offer(&p.key.serial, &d, e, stick.as_deref()) {
        Ok(o) => o,
        Err(w) => return why(&w),
    };
    match plan_on(&p.disk, p.number, o) {
        Ok(Plan::Make { remake }) if remake == p.remake => {}
        Ok(_) => return refused("changed", "the partition is not what it was when you were asked: nothing was written"),
        Err(answer) => return answer,
    }
    let Some(value) = p.key.render() else { return why(&Why::BadKey("the serial cannot stand in a CONFIG line")) };
    if MAKING.swap(true, Ordering::AcqRel) {
        return refused("window_busy", "another bitsdisk is making a pool right now");
    }
    drv::arm(&*c, nsid, &d, &w);
    let r = make(&part, &value);
    // The window was for this; the next boot checks and opens it again by itself.
    drv::disarm(&*c, &d);
    MAKING.store(false, Ordering::Release);
    match r {
        Ok(()) => {
            klog::event!(Info, "nvme", "setup.done", disk = p.disk.as_str(), number = p.number, remake = p.remake; "bitsdisk: CONFIG.TXT has nvme_data=, the pool is made, a restart brings /data onto it");
            String::from("ok done\n")
        }
        Err(e) => {
            klog::event!(Error, "nvme", "setup.failed", disk = p.disk.as_str(), error = e.as_str());
            refused("failed", &e)
        }
    }
}

fn make(part: &Arc<Disk>, value: &str) -> Result<(), String> {
    write_key(value).map_err(|e| format!("CONFIG.TXT could not be written (errno {e}); nothing was made"))?;
    fs_ext4::data::format_pool(Arc::new(DiskDevice(part.clone())), b"bitsdata").map_err(|e| format!("making the pool failed (errno {e}); CONFIG.TXT has the key, bitsdisk can make it again"))?;
    part.flush(devmodel::block::IoPriority::Normal).map_err(|e| format!("the flush failed: {}", e.as_str()))
}

/// One command.
pub fn command(line: &str) -> String {
    let mut w = line.split_ascii_whitespace();
    match (w.next(), w.next(), w.next(), w.next()) {
        (Some("list"), None, ..) => list(),
        (Some("prepare"), Some(d), Some(n), None) => match n.parse::<u32>() {
            Ok(n) => prepare(d, n),
            Err(_) => refused("bad_command", "the partition number is not a number"),
        },
        (Some("confirm"), None, ..) => confirm(),
        (Some("cancel"), None, ..) => {
            PENDING.lock().take();
            String::from("ok\n")
        }
        _ => refused("bad_command", "list, prepare <disk> <number>, confirm or cancel"),
    }
}

/// A process MCP's `run` started, or any of its descendants.
fn from_mcp() -> bool {
    proc::current().is_some_and(|p| p.program == proc::MCP_PROGRAM)
}

struct Session {
    answer: ksync::Mutex<(Vec<u8>, usize)>,
}

impl File for Session {
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        let mut a = self.answer.lock();
        let n = buf.len().min(a.0.len() - a.1);
        buf[..n].copy_from_slice(&a.0[a.1..a.1 + n]);
        a.1 += n;
        Ok(n)
    }
    fn write_at(&self, _pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        if from_mcp() {
            return Err(EPERM);
        }
        if buf.len() > LINE_MAX {
            return Err(EINVAL);
        }
        let Ok(line) = core::str::from_utf8(buf) else { return Err(EINVAL) };
        let text = command(line);
        *self.answer.lock() = (text.into_bytes(), 0);
        Ok(buf.len())
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o600
    }
    fn rdev(&self) -> u64 {
        vfs::abi::encode_dev(DEV.0, DEV.1)
    }
}

/// Opens refused, for the ktest that opens as root with MCP's program id.
pub static OPEN_REFUSED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

struct Node;

impl File for Node {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EBADF)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EBADF)
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o600
    }
    fn rdev(&self) -> u64 {
        vfs::abi::encode_dev(DEV.0, DEV.1)
    }
    fn open_node(&self, _flags: u64, who: vfs::Cred) -> Option<Result<Arc<dyn File>, Errno>> {
        if who.uid != 0 || from_mcp() {
            OPEN_REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            klog::event!(Warn, "nvme", "setup.open_refused", uid = who.uid, mcp = from_mcp(); "bitsdisk's path is for root at the machine, never over MCP");
            return Some(Err(EPERM));
        }
        Some(Ok(Arc::new(Session { answer: ksync::Mutex::named("nvme.setup.answer", (Vec::new(), 0)) })))
    }
}

fn node() -> Arc<dyn File> {
    Arc::new(Node)
}
vfs::devnode!("/dev/bitsos/disksetup", node);
