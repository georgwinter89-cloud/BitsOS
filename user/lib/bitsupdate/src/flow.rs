//! The update itself, for the page (bitspkgd's update ops) and the console (`bitsupdate`): what is installed,
//! whether a newer version is there (its signed manifest and "Was ist neu" fetched and checked when the page
//! opens), the download and its checks, and the hand-over to the kernel. Nothing is written to the boot medium
//! before every check passed; nothing runs without being asked.

use crate::fetchrun::{Fetcher, Got};
use crate::github::{self, Release};
use crate::manifest::{self, MANIFEST, MANIFEST_MAX, MANIFEST2, MANIFEST2_MAX, Manifest, NOTES, SIGNATURE, SIGNATURE2};
use crate::parts::{self, Kind, Part, Plan, Room};
use crate::node::Node;
use crate::verify::{self, Why};
use mcp_wire::json::Json;
use std::path::PathBuf;

/// Every way an update stops, each with Georg's line (`text`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fail {
    NoToken,
    Network(String),
    TokenWrong,
    TokenNoAccess,
    Clock,
    Tls(String),
    Server(String),
    NoRelease,
    Check(Why),
    Kernel(String),
    NoSpace,
    /// UPD-4: the version does not fit on the medium (checked before any download): bytes needed at most, free
    /// (with what the update may remove), missing.
    NoRoom { need: u64, free: u64, missing: u64 },
    /// The download does not fit in memory (the work folder is in RAM): needed, free.
    NoRam { need: u64, free: u64 },
    Local(String),
    /// The kernel's node refused this process: only root at this machine's keyboard (or bitspkgd for the page), never
    /// a process MCP's run started (GP-R3: the Mini-PC's check ran `bitsupdate status` over MCP).
    Refused(String),
    /// "Zurück" or "Wieder die neueste" refused (the kernel's reason as detail).
    Pin(String),
    /// The release is not above the medium's newest folder and is not what runs (see `Taken`).
    Taken(Taken),
}

/// Why a release at or below the medium's newest folder is not offered, though the running build is older or
/// has no number: the page must not say "neueste Version" then (same number = same code).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Taken {
    /// The release is on the medium and starts at the next restart (installed, not tried yet).
    Waiting(u32),
    /// The release is on the medium and was confirmed, but an older one runs (Zurück chosen).
    Back { version: u32, running: u32 },
    /// The release is on the medium but its start did not come through (it fell back).
    Failed(u32),
    /// Its slot (or a higher one) holds another build: a development build took the number.
    Occupied { version: u32, newest: u32 },
}

impl Fail {
    pub fn text(&self) -> String {
        crate::text::fail(self)
    }
    /// The technical detail for the log and the desk (never Georg's line: `text` is).
    pub fn detail(&self) -> Option<&str> {
        match self {
            Fail::Network(t) | Fail::Tls(t) | Fail::Server(t) | Fail::Kernel(t) | Fail::Local(t) | Fail::Refused(t) | Fail::Pin(t) => Some(t.as_str()),
            _ => None,
        }
    }
}

fn got(g: Got) -> Fail {
    match g {
        Got::Status(401) => Fail::TokenWrong,
        Got::Status(403 | 404) => Fail::TokenNoAccess,
        Got::Status(c) => Fail::Server(format!("HTTP status {c}")),
        Got::Network(e) => Fail::Network(e),
        Got::Tls(e) => Fail::Tls(e),
        Got::Clock => Fail::Clock,
        Got::TooLarge => Fail::Server(String::from("a file is larger than announced")),
        Got::Other(e) => Fail::Local(e),
    }
}

/// What the medium and the running system say (the kernel node's `status`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Installed {
    pub running: u32,
    pub date: String,
    pub generation: u32,
    pub generations: Vec<(u32, String)>,
    pub newest: u32,
    pub pin: Option<u32>,
    pub back: Option<u32>,
    pub next: String,
    pub writable: bool,
    pub pending: bool,
    pub confirm: String,
    /// VB-1: the kernel's confirmation as one word (pending, writing, confirmed, failed, replaced, none), whether
    /// it asks Georg (`confirm::decide`), and which proof or answer wrote OK.
    pub state: String,
    pub asks: bool,
    /// Georg's "Ja" is with the kernel and not written yet (it may still fail: the question comes back then).
    pub answered: bool,
    pub by: String,
}

pub fn parse_status(j: &Json) -> Installed {
    let r = j.get("running");
    let num = |v: Option<&Json>| v.and_then(Json::as_u64).unwrap_or(0) as u32;
    let m = j.get("medium");
    let gens: Vec<(u32, String)> = m
        .and_then(|m| m.get("generations"))
        .and_then(Json::as_array)
        .map(|a| a.iter().map(|g| (num(g.get("n")), g.get("markers").and_then(Json::as_str).unwrap_or("").to_string())).collect())
        .unwrap_or_default();
    let c = j.get("confirm");
    Installed {
        running: num(r.and_then(|r| r.get("version"))),
        date: r.and_then(|r| r.get("date")).and_then(Json::as_str).unwrap_or("").to_string(),
        generation: num(r.and_then(|r| r.get("generation"))),
        newest: gens.iter().map(|g| g.0).max().unwrap_or(0),
        generations: gens,
        pin: m.and_then(|m| m.get("pin")).and_then(Json::as_u64).map(|p| p as u32),
        back: m.and_then(|m| m.get("back")).and_then(Json::as_u64).map(|p| p as u32),
        next: m.and_then(|m| m.get("next")).and_then(Json::as_str).unwrap_or("").to_string(),
        writable: m.and_then(|m| m.get("writable")).and_then(Json::as_bool).unwrap_or(false),
        pending: c.and_then(|c| c.get("pending")).and_then(Json::as_bool).unwrap_or(false),
        confirm: c.and_then(|c| c.get("text")).and_then(Json::as_str).unwrap_or("").to_string(),
        state: c.and_then(|c| c.get("state")).and_then(Json::as_str).unwrap_or("").to_string(),
        asks: c.and_then(|c| c.get("asks")).and_then(Json::as_bool).unwrap_or(false),
        answered: c.and_then(|c| c.get("answered")).and_then(Json::as_bool).unwrap_or(false),
        by: c.and_then(|c| c.get("by")).and_then(Json::as_str).unwrap_or("").to_string(),
    }
}

/// The kernel's install node, or why not.
pub fn node() -> Result<Node, Fail> {
    opened(Node::open())
}

/// EPERM (a process MCP's run started) and EACCES (not root) are both PermissionDenied.
pub fn opened(r: std::io::Result<Node>) -> Result<Node, Fail> {
    r.map_err(|e| {
        let d = format!("{}: {e}", crate::node::path().display());
        if e.kind() == std::io::ErrorKind::PermissionDenied { Fail::Refused(d) } else { Fail::Local(d) }
    })
}

pub fn status() -> Result<Installed, Fail> {
    let mut n = node()?;
    let j = n.command("status").map_err(Fail::Local)?;
    Ok(parse_status(&j))
}

/// Where the update works: the source, the token, the key, a scratch folder.
pub struct Ctx {
    pub dir: PathBuf,
    /// Where the token is (`source::token_dir`).
    pub token_dir: PathBuf,
    pub work: PathBuf,
    pub key: Option<[u8; 32]>,
    /// The fetch program (`/bin/fetch`).
    pub fetch: PathBuf,
}

impl Ctx {
    pub fn standard() -> Ctx {
        let work = std::env::var_os("BITSUPDATE_WORK").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/run/bitsos/update"));
        Ctx { dir: crate::source::dir(), token_dir: crate::source::token_dir(), work, key: crate::sig::DESK_KEY, fetch: PathBuf::from("/bin/fetch") }
    }
}

/// A newer version, its manifest and notes checked against the desk's key.
#[derive(Clone, Debug, PartialEq)]
pub struct Offer {
    pub release: Release,
    pub manifest: Manifest,
    pub manifest_bytes: Vec<u8>,
    pub sig: Vec<u8>,
    pub notes: String,
    /// UPD-4: a version with parts (form 2): what the medium holds of them and what comes.
    pub plan: Option<Plan>,
}

/// What the kernel gets, checked: the two files in memory, the parts' assets in the work folder.
pub struct Bundle {
    pub version: u32,
    pub kernel: Vec<u8>,
    pub initrd: Vec<u8>,
    /// Every part of the version with its downloaded assets (none for a part the medium holds).
    pub parts: Vec<(Part, Vec<PathBuf>)>,
    dir: Option<PathBuf>,
}

impl Drop for Bundle {
    fn drop(&mut self) {
        if let Some(d) = &self.dir {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

struct Session<'a> {
    cx: &'a Ctx,
    src: crate::source::Source,
    token: String,
    f: Fetcher,
    dl: PathBuf,
    /// The folder outlives the session (a bundle's assets wait in it for the install).
    keep: bool,
}

impl<'a> Session<'a> {
    fn new(cx: &'a Ctx) -> Result<Session<'a>, Fail> {
        let src = crate::source::source(&cx.dir);
        let token = crate::source::read_token(&cx.token_dir).ok_or(Fail::NoToken)?;
        let mut f = Fetcher::standard(cx.fetch.clone(), src.ca.clone());
        // One folder per session: bitspkgd may check (a page opened) while an update downloads.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dl = cx.work.join(format!("dl-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&cx.work).map_err(|e| Fail::Local(format!("{}: {e}", cx.work.display())))?;
        sweep(&cx.work, &|pid| std::path::Path::new(&format!("/proc/{pid}")).exists());
        f.folder(&dl).map_err(Fail::Local)?;
        // A CA of SOURCE.TXT sits in the root-only UPDATE folder: fetch (nobody) gets its own copy.
        if let Some(ca) = &src.ca {
            let mine = dl.join("ca.pem");
            std::fs::copy(ca, &mine).map_err(|e| Fail::Local(format!("{ca}: {e}")))?;
            if let Some((u, g)) = f.drop_to {
                std::os::unix::fs::chown(&mine, Some(u), Some(g)).map_err(|e| Fail::Local(format!("{}: {e}", mine.display())))?;
            }
            f.ca = Some(mine.display().to_string());
        }
        Ok(Session { cx, src, token, f, dl, keep: false })
    }

    fn get(&self, url: &str, name: &str, accept: &str, max: u64) -> Result<Vec<u8>, Fail> {
        let out = self.dl.join(name);
        self.f.get(url, &out, accept, Some(&self.token), max).map_err(got)?;
        let b = std::fs::read(&out).map_err(|e| Fail::Local(format!("{}: {e}", out.display())))?;
        let _ = std::fs::remove_file(&out);
        Ok(b)
    }

    fn asset(&self, r: &Release, name: &str, max: u64) -> Result<Vec<u8>, Fail> {
        let a = r.asset(name).ok_or_else(|| Fail::Check(Why::File { name: name.into(), what: String::from("fehlt in der Version") }))?;
        if a.size > max {
            return Err(Fail::Check(Why::File { name: name.into(), what: format!("{} bytes, more than {max}", a.size) }));
        }
        self.get(&github::asset_url(&self.src.api, &self.src.repo, a.id), name, github::ASSET_ACCEPT, max)
    }

    /// Asset `name` into the session's folder (not read into memory): checked against `e` there.
    fn asset_file(&self, r: &Release, name: &str, e: &manifest::Entry) -> Result<PathBuf, Fail> {
        let a = r.asset(name).ok_or_else(|| Fail::Check(Why::File { name: name.into(), what: String::from("fehlt in der Version") }))?;
        if a.size != e.size {
            return Err(Fail::Check(Why::File { name: name.into(), what: format!("{} bytes in the release, the manifest says {}", a.size, e.size) }));
        }
        let out = self.dl.join(name);
        self.f.get(&github::asset_url(&self.src.api, &self.src.repo, a.id), &out, github::ASSET_ACCEPT, Some(&self.token), e.size).map_err(got)?;
        crate::partsio::check_file(e, &out)?;
        Ok(out)
    }
}

/// Removes the work folders of processes that are gone (killed between download and install): they lie in RAM.
pub fn sweep(work: &std::path::Path, alive: &dyn Fn(u32) -> bool) {
    let Ok(d) = std::fs::read_dir(work) else { return };
    for e in d.flatten() {
        let name = e.file_name();
        let pid = name.to_str().and_then(|n| n.strip_prefix("dl-")).and_then(|r| r.split_once('-')).and_then(|(p, _)| p.parse::<u32>().ok());
        if let Some(pid) = pid
            && pid != std::process::id()
            && !alive(pid)
        {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.dl);
        }
    }
}

/// The kernel node's `plan <version>`.
pub fn room(version: u32) -> Result<Room, Fail> {
    let mut n = node()?;
    let a = n.command(&format!("plan {version}")).map_err(Fail::Local)?;
    parts::room(&a).map_err(Fail::Local)
}

/// The newest release, if it is newer than `have`: its manifest, signature and notes fetched and checked.
pub fn check(cx: &Ctx, have: &Installed) -> Result<Option<Offer>, Fail> {
    let s = Session::new(cx)?;
    let list = s.get(&github::list_url(&s.src.api, &s.src.repo), "releases.json", github::JSON_ACCEPT, github::LIST_MAX)?;
    let Some(r) = github::newest(&list).map_err(Fail::Server)? else { return Err(Fail::NoRelease) };
    if r.version <= have.newest.max(have.running) {
        return not_newer(r.version, have, &mut |n| slot(n));
    }
    // UPD-4: form 2 when the release has it (the parts); MANIFEST is the form the versions before UPD-4 read.
    let form2 = r.asset(MANIFEST2).is_some();
    if !form2 {
        withheld(&r.assets.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), &|| room(r.version).ok())?;
    }
    let (mb, sig) = if form2 {
        (s.asset(&r, MANIFEST2, MANIFEST2_MAX as u64)?, s.asset(&r, SIGNATURE2, 1024)?)
    } else {
        (s.asset(&r, MANIFEST, MANIFEST_MAX as u64)?, s.asset(&r, SIGNATURE, 1024)?)
    };
    let m = verify::manifest(cx.key.as_ref(), &mb, &sig, r.version, have.newest, have.running).map_err(Fail::Check)?;
    if form2 && !m.has_parts() {
        return Err(Fail::Check(Why::PartsWithheld));
    }
    let notes = match m.entry(NOTES) {
        Some(e) => {
            let b = s.asset(&r, NOTES, e.size)?;
            verify::file(&m, NOTES, &b).map_err(Fail::Check)?;
            verify::notes(&b)
        }
        None => String::new(),
    };
    let _ = s.cx;
    let plan = if m.has_parts() { Some(parts::plan(&m, &room(m.version)?)) } else { None };
    Ok(Some(Offer { release: r, manifest: m, manifest_bytes: mb, sig, notes, plan }))
}

/// A release without MANIFEST2 (pure; `room`: the node's plan, None when there is no node to ask): refused when
/// it still carries part assets (MANIFEST2 taken out) or when a slot of this medium runs with parts already. Both
/// are a release whose parts were withheld: its kernel would run with the medium's own, older bin and trees.
pub fn withheld(assets: &[&str], room: &dyn Fn() -> Option<Room>) -> Result<(), Fail> {
    let parts_left = assets.iter().any(|a| *a == SIGNATURE2 || a.starts_with("bin.") || a.starts_with("tree."));
    if parts_left || room().is_some_and(|r| r.in_use) {
        return Err(Fail::Check(Why::PartsWithheld));
    }
    Ok(())
}

/// A slot on the medium: the version its KERNEL carries (0: a development build or older) and its markers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    pub version: u32,
    pub markers: String,
}

/// The kernel node's `slot <n>` (None: no such folder).
pub fn slot(n: u32) -> Result<Option<Slot>, Fail> {
    let mut node = node()?;
    let a = node.command(&format!("slot {n}")).map_err(Fail::Local)?;
    if a.get("ok").and_then(Json::as_bool) != Some(true) {
        return Err(Fail::Local(a.get("failed").or_else(|| a.get("bad_request")).and_then(Json::as_str).unwrap_or("slot: no answer").to_string()));
    }
    if a.get("present").and_then(Json::as_bool) != Some(true) {
        return Ok(None);
    }
    Ok(Some(Slot {
        version: a.get("version").and_then(Json::as_u64).unwrap_or(0) as u32,
        markers: a.get("markers").and_then(Json::as_str).unwrap_or("").to_string(),
    }))
}

/// A release `v` not above the medium's newest folder or the running version (pure; `slot` asks the medium).
/// "neueste Version" only when the running build is numbered and at least `v`; otherwise the slot of `v` says
/// whether `v` is installed (waiting, gone back from, failed) or its number is taken by another build.
pub fn not_newer(v: u32, have: &Installed, slot: &mut dyn FnMut(u32) -> Result<Option<Slot>, Fail>) -> Result<Option<Offer>, Fail> {
    if have.running != 0 && v <= have.running {
        return Ok(None);
    }
    let t = match slot(v)? {
        Some(s) if s.version == v && s.markers.contains("OK") => Taken::Back { version: v, running: have.running },
        Some(s) if s.version == v && s.markers == "NEW" => Taken::Waiting(v),
        Some(s) if s.version == v => Taken::Failed(v),
        _ => Taken::Occupied { version: v, newest: have.newest },
    };
    Err(Fail::Taken(t))
}

/// Which piece of a version a step is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum What {
    Kernel,
    Initrd,
    /// The bin part: the GPU desktop and the programs' starters.
    Desktop,
    Program(String),
}

impl What {
    pub fn of(part: &str) -> What {
        if part == crate::parts_rules::BIN { What::Desktop } else { What::Program(part.to_string()) }
    }
}

/// Steps the page and the console show while an update runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Checking,
    Downloading { what: What, bytes: u64 },
    Verifying,
    /// UPD-4: a part goes onto the medium.
    Writing { what: What, bytes: u64 },
    Installing,
}

/// Downloads what `o` brings and checks each file against the signed manifest (checked again first): KERNEL and
/// INITRD into memory, every part the medium lacks into the work folder. Before the first byte: whether the version
/// fits on the medium and the download in memory.
pub fn download(cx: &Ctx, have: &Installed, o: &Offer, step: &mut dyn FnMut(Step)) -> Result<Bundle, Fail> {
    step(Step::Checking);
    let m = verify::manifest(cx.key.as_ref(), &o.manifest_bytes, &o.sig, o.release.version, have.newest, have.running).map_err(Fail::Check)?;
    let plan = if m.has_parts() {
        let room = room(m.version)?;
        let p = parts::plan(&m, &room);
        if let Err(missing) = parts::fits(&p, &room) {
            return Err(Fail::NoRoom { need: parts::need(&p, &room), free: room.free.saturating_add(room.orphans).saturating_add(room.freed), missing });
        }
        Some(p)
    } else {
        None
    };
    if let Some(p) = &plan {
        // The bundle's two files in memory and the parts' assets in the work folder (RAM), and a reserve; not
        // known is not enough (several GB must not start blind).
        let need = p.download().saturating_add(64 << 20);
        let free = crate::partsio::ram_free().unwrap_or(0);
        if free < need {
            return Err(Fail::NoRam { need, free });
        }
    }
    let mut s = Session::new(cx)?;
    let mut files = Vec::new();
    for (name, what) in [("KERNEL", What::Kernel), ("INITRD", What::Initrd)] {
        let e = m.entry(name).ok_or_else(|| Fail::Check(Why::File { name: name.into(), what: String::from("not in the manifest") }))?;
        step(Step::Downloading { what, bytes: e.size });
        let b = s.asset(&o.release, name, e.size)?;
        step(Step::Verifying);
        verify::file(&m, name, &b).map_err(Fail::Check)?;
        files.push(b);
    }
    let initrd = files.pop().unwrap_or_default();
    let kernel = files.pop().unwrap_or_default();
    let mut got = Vec::new();
    if let Some(p) = &plan {
        for (part, present) in p.parts.iter().zip(&p.present) {
            let mut paths = Vec::new();
            if !present {
                step(Step::Downloading { what: What::of(&part.name), bytes: part.download_bytes() });
                for e in &part.assets {
                    let asset = if part.kind == Kind::Bin { manifest::bin_asset(&e.name) } else { e.name.clone() };
                    paths.push(s.asset_file(&o.release, &asset, &manifest::Entry { name: asset.clone(), ..e.clone() })?);
                }
            }
            got.push((part.clone(), paths));
        }
    }
    s.keep = true;
    Ok(Bundle { version: m.version, kernel, initrd, parts: got, dir: Some(s.dl.clone()) })
}

/// What the kernel did with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Done {
    pub slot: u32,
    pub next: String,
}

/// Writes the parts the medium lacks, names every part of the version for the new slot, then the bundle: one
/// session of the node, so its parts are held from the first write to the slot's FILES.
pub fn install(b: &Bundle, step: &mut dyn FnMut(Step)) -> Result<Done, Fail> {
    let mut n = node()?;
    // Each part named right after it is there: from then on no cleanup of a later part removes it.
    for (p, files) in &b.parts {
        crate::partsio::send(&mut n, p, files, step)?;
        n.use_part(&p.name, &p.hash).map_err(crate::partsio::node_fail)?;
    }
    step(Step::Installing);
    let a = n.install(b.version, &b.kernel, &b.initrd).map_err(Fail::Local)?;
    kernel_answer(&a)
}

/// The node's answer to `install` (pure).
pub fn kernel_answer(a: &Json) -> Result<Done, Fail> {
    if a.get("ok").and_then(Json::as_bool) == Some(true) {
        return Ok(Done {
            slot: a.get("installed").and_then(Json::as_u64).unwrap_or(0) as u32,
            next: a.get("next").and_then(Json::as_str).unwrap_or("").to_string(),
        });
    }
    let t = ["refused", "failed", "bad_request"].iter().find_map(|k| a.get(k).and_then(Json::as_str)).unwrap_or("no reason given").to_string();
    if t.contains("KiB free") {
        return Err(Fail::NoSpace);
    }
    Err(Fail::Kernel(t))
}

/// "Zurück" and "Wieder die neueste": the PIN through the kernel.
pub fn pin(back: bool) -> Result<String, Fail> {
    let mut n = node()?;
    let a = n.command(if back { "back" } else { "forward" }).map_err(Fail::Local)?;
    if a.get("ok").and_then(Json::as_bool) == Some(true) {
        let next = a.get("next").and_then(Json::as_u64).unwrap_or(0);
        return Ok(next.to_string());
    }
    Err(Fail::Pin(a.get("refused").and_then(Json::as_str).unwrap_or("no reason given").to_string()))
}
