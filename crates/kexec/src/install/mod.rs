//! RB-6: `install_kernel`, the remote install chain's back end (V2-PLAN §4 "install_kernel", §7, §7.1), and the
//! kernel's confirmation of its own boot.
//!
//! An install checks the bundle as bitsboot will (`check`), plans the generation (`plan`), then changes the stick
//! only through FS-7's narrow SYS API (`fs_fat::sysapi`, on the identified boot stick mounted read-write, every
//! call durable when it returns): the unconfirmed newest generation is removed, pruning and an asked-for unpin
//! happen, `\BITSOS2\SYS\<n>\` is made, INITRD then KERNEL written, both read back from the stick and compared
//! byte for byte, and only then `NEW` set. A cut at any point leaves a stick bitsboot handles: a folder without
//! `NEW` and with a cut-short KERNEL fails bitsboot's checks (fallback at once); a complete but unmarked one gets
//! one try like `NEW` and then the fallback; the next install replaces either.
//!
//! The confirmation (`confirm`): a kernel bitsboot started as the one try of a new generation (`NEWEST_TRY`,
//! started by the firmware, not by a Reload) writes `OK` and clears `TRY`/`NEW` once MCP has answered requests
//! over 2 minutes (CONFIG `boot_confirm_s=`).
//!
//! Every stick operation of this module runs under one lock (`kexec.install`), so an install and the
//! confirmation never interleave.

pub mod check;
pub mod confirm;
pub mod plan;
pub mod system;
pub mod system_stick;
pub mod system_tool;
mod tool;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod system_tests;
mod system_ktests;
mod ktests;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use fs_fat::sysapi::Marker;
use plan::{ConfirmPlan, Gen, InstallPlan, Stale};

/// Everything this module does on the stick goes through this (the FS-7 SYS API on the real stick; a model in
/// the host tests). Errors are errno values.
pub trait Stick {
    fn generations(&self) -> Result<Vec<Gen>, i64>;
    fn pin(&self) -> Result<Option<u32>, i64>;
    fn create_generation(&self, n: u32) -> Result<(), i64>;
    fn write_file(&self, n: u32, name: &str, data: &[u8]) -> Result<(), i64>;
    /// Up to `buf.len()` bytes of `name` at `off` (below the end: a multiple of 512) as the stick holds it; 0 at the end.
    fn read_at(&self, n: u32, name: &str, off: u64, buf: &mut [u8]) -> Result<usize, i64>;
    fn set_marker(&self, n: u32, m: Marker, on: bool) -> Result<(), i64>;
    fn set_pin(&self, n: Option<u32>) -> Result<(), i64>;
    fn remove_generation(&self, n: u32) -> Result<(), i64>;
    /// (cluster bytes, free bytes).
    fn space(&self) -> Result<(u64, u64), i64>;
    /// `\BITSOS2\CONFIG.TXT` as the stick holds it (None: no such file).
    fn config(&self) -> Result<Option<Vec<u8>>, i64>;
    /// Replaces `\BITSOS2\CONFIG.TXT` (crash-safe: a sibling, then renamed over it), durable.
    fn write_config(&self, data: &[u8]) -> Result<(), i64>;
}

/// The lock every stick operation of `install`/`confirm` holds (sleeping; an install takes seconds).
pub static LOCK: ksync::Mutex<()> = ksync::Mutex::named("kexec.install", ());

pub fn errno_name(e: i64) -> String {
    use uapi::errno::*;
    let n = match e {
        EROFS => "EROFS (the stick is read-only)",
        ENODEV => "ENODEV (no boot stick mounted)",
        ENOSPC => "ENOSPC (the stick is full)",
        EEXIST => "EEXIST",
        ENOENT => "ENOENT",
        EPERM => "EPERM (refused by the stick's rules)",
        EIO => "EIO (the stick did not do it)",
        ENOMEM => "ENOMEM",
        EFBIG => "EFBIG",
        EINVAL => "EINVAL",
        ENOTEMPTY => "ENOTEMPTY",
        _ => return format!("errno {e}"),
    };
    String::from(n)
}

fn gen_from(g: &fs_fat::sysapi::Generation) -> Gen {
    let has = |n: &str| g.files.iter().any(|(f, _)| f == n);
    Gen { n: g.n, new: g.new, tried: g.try_, ok: g.ok, kernel: has("KERNEL"), initrd: has("INITRD"), bytes: g.files.iter().map(|f| f.1).sum() }
}

const CONFIG_PATH: &str = "/BITSOS2/CONFIG.TXT";
const CONFIG_TEMP: &str = "/BITSOS2/CONFIG.NEW";

/// The real stick: FS-7's SYS API at `/boot` (`fs_fat::stick::with_sys`).
pub struct FatStick<'a>(pub &'a fs_fat::sysapi::Sys<'a>);

impl Stick for FatStick<'_> {
    fn generations(&self) -> Result<Vec<Gen>, i64> {
        Ok(self.0.generations()?.iter().map(gen_from).collect())
    }
    fn pin(&self) -> Result<Option<u32>, i64> {
        self.0.pin()
    }
    fn create_generation(&self, n: u32) -> Result<(), i64> {
        self.0.create_generation(n)
    }
    fn write_file(&self, n: u32, name: &str, data: &[u8]) -> Result<(), i64> {
        self.0.write_file(n, name, data)
    }
    fn read_at(&self, n: u32, name: &str, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        self.0.read_at(n, name, off, buf)
    }
    fn set_marker(&self, n: u32, m: Marker, on: bool) -> Result<(), i64> {
        self.0.set_marker(n, m, on)
    }
    fn set_pin(&self, n: Option<u32>) -> Result<(), i64> {
        self.0.set_pin(n)
    }
    fn remove_generation(&self, n: u32) -> Result<(), i64> {
        self.0.remove_generation(n)
    }
    fn space(&self) -> Result<(u64, u64), i64> {
        let (cb, _, free) = self.0.vol.statfs()?;
        Ok((cb, cb.saturating_mul(free)))
    }
    fn config(&self) -> Result<Option<Vec<u8>>, i64> {
        let path = format!("{}{CONFIG_PATH}", self.0.mount);
        match self.0.ns.read_file(&path, fs_fat::protect::CONFIG_MAX as usize) {
            Ok(t) => Ok(Some(t)),
            Err(uapi::errno::ENOENT) => Ok(None),
            Err(e) => Err(e),
        }
    }
    fn write_config(&self, data: &[u8]) -> Result<(), i64> {
        use vfs::abi::{O_CREAT, O_TRUNC, O_WRONLY};
        let c = vfs::ops::Ctx::kernel(self.0.ns);
        let (path, temp) = (format!("{}{CONFIG_PATH}", self.0.mount), format!("{}{CONFIG_TEMP}", self.0.mount));
        let f = c.open(None, temp.as_bytes(), O_WRONLY | O_CREAT | O_TRUNC, 0o644)?;
        let mut done = 0;
        while done < data.len() {
            let w = f.pwrite(done as u64, &data[done..])?;
            if w == 0 {
                return Err(uapi::errno::EIO);
            }
            done += w;
        }
        f.inode().ok_or(uapi::errno::EIO)?.sync(vfs::pagecache::SyncKind::Fsync)?;
        drop(f);
        // FS-7's rename over an existing name clears the sibling's entry, then rewrites CONFIG.TXT's entry in one
        // sector: a cut leaves the old CONFIG.TXT or the new one, never none. Its CONFIG guard runs again here.
        c.rename(None, temp.as_bytes(), None, path.as_bytes(), 0)?;
        self.0.vol.sync(true)
    }
}

/// What the caller asked besides the bundle.
#[derive(Clone, Copy, Debug, Default)]
pub struct Opts {
    /// Remove `\BITSOS2\SYS\PIN` so the new generation starts (B1: "the next confirmed install" ends a pin).
    pub unpin: bool,
    /// Install although no confirmed generation would stay as the fallback.
    pub allow_no_fallback: bool,
    /// Check and plan only.
    pub dry_run: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallError {
    /// Refused before the stick was changed (bundle, CONFIG, rules, space).
    Refused(String),
    /// The stick was changed and then something failed; the text says what the stick holds now.
    Failed(String),
}

impl InstallError {
    pub fn text(&self) -> &str {
        match self {
            InstallError::Refused(t) | InstallError::Failed(t) => t,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub plan: InstallPlan,
    pub kernel: Option<check::KernelFacts>,
    pub initrd: Option<check::InitrdFacts>,
    pub sys_keep: Option<u32>,
    pub config_written: bool,
    /// What bitsboot starts at the next boot (after this install), as `bitsboot_pure::gens::choose` says.
    pub next_boot: String,
    pub warnings: Vec<String>,
}

/// Rounded up to whole clusters, plus one cluster for the folder's directory.
fn on_stick(bytes: u64, cluster: u64) -> u64 {
    let c = cluster.max(512);
    bytes.div_ceil(c) * c
}

fn next_boot_text(st: &dyn Stick) -> String {
    let (Ok(gens), Ok(pin)) = (st.generations(), st.pin()) else { return String::from("unknown (the stick could not be listed)") };
    match plan::next_boot(&gens, pin) {
        Ok((n, reason, why)) => format!("generation {n} ({reason}: {why})"),
        Err(t) => format!("nothing: {t}"),
    }
}

fn stick_err(what: &str, e: i64) -> String {
    format!("{what}: {}", errno_name(e))
}

/// Checks the bundle and installs it as a new generation on `st` (see the module text). The caller holds `LOCK`.
pub fn install_on(st: &dyn Stick, kernel: &[u8], initrd: &[u8], config: Option<&[u8]>, o: &Opts) -> Result<Report, InstallError> {
    let refused = InstallError::Refused;
    let kf = check::kernel(kernel).map_err(refused)?;
    let inf = check::initrd(initrd).map_err(refused)?;
    let gens = st.generations().map_err(|e| refused(stick_err("listing \\BITSOS2\\SYS", e)))?;
    let pin = st.pin().map_err(|e| refused(stick_err("reading \\BITSOS2\\SYS\\PIN", e)))?;
    let live = st.config().map_err(|e| refused(stick_err("reading \\BITSOS2\\CONFIG.TXT", e)))?;
    // WL-4 CR 4: read_file shows secrets as `***`; a CONFIG.TXT built from that read keeps the stick's values.
    let kept = config.map(|new| fs_fat::protect::unredact_config(live.as_deref().unwrap_or(b""), new));
    let config = kept.as_deref();
    if let Some(new) = config {
        check::config(live.as_deref(), new).map_err(refused)?;
    }
    let mut warnings = Vec::new();
    // The CONFIG.TXT the stick will hold after this install decides how many are kept.
    let keep = match check::sys_keep(config.or(live.as_deref())) {
        Ok(k) => Some(k),
        Err(w) => {
            warnings.push(w);
            None
        }
    };
    let mut p = plan::install(&gens, pin, keep.unwrap_or(1), o.unpin, o.allow_no_fallback).map_err(|e| refused(e.describe()))?;
    if keep.is_none() {
        p.prune.clear();
        p.freed = gens.iter().filter(|g| Some(g.n) == p.replaced).map(|g| g.bytes).sum();
    }
    let (cluster, free) = st.space().map_err(|e| refused(stick_err("reading the stick's free space", e)))?;
    let need = on_stick(kf.bytes, cluster) + on_stick(inf.bytes, cluster) + 4 * cluster.max(512);
    if free.saturating_add(p.freed) < need {
        return Err(refused(format!(
            "the stick has {} KiB free (+{} KiB from the generations this install removes) and the bundle needs {} KiB: lower sys_keep or remove files below \\BITSOS2\\",
            free >> 10,
            p.freed >> 10,
            need >> 10
        )));
    }
    if let Some(q) = p.pinned_instead {
        warnings.push(format!(
            "\\BITSOS2\\SYS\\PIN names generation {q}: bitsboot keeps starting it, not the new generation, until the pin goes (unpin=true)"
        ));
    }
    let mut r = Report { plan: p.clone(), kernel: Some(kf.clone()), initrd: Some(inf.clone()), sys_keep: keep, warnings, ..Report::default() };
    if o.dry_run {
        r.next_boot = String::from("unchanged (dry run)");
        return Ok(r);
    }

    // From here the stick changes. Each removal is a state bitsboot handles (see the module text).
    let failed = |what: String| InstallError::Failed(format!("{what}; the stick now holds: {}", listing(st)));
    if let Some(old) = p.replaced {
        st.remove_generation(old).map_err(|e| failed(stick_err(&format!("removing the unconfirmed generation {old}"), e)))?;
        confirm::note_replaced(old);
        klog::event!(Info, "kexec", "install.replaced", generation = old; "the unconfirmed newest generation was removed");
    }
    if o.unpin && pin.is_some() {
        st.set_pin(None).map_err(|e| failed(stick_err("removing \\BITSOS2\\SYS\\PIN", e)))?;
        klog::event!(Info, "kexec", "install.unpinned", was = pin.unwrap_or(0));
    }
    for &g in &p.prune {
        // OK first, so a cut never leaves a fallback with half its files.
        st.set_marker(g, Marker::Ok, false)
            .and_then(|()| st.remove_generation(g))
            .map_err(|e| failed(stick_err(&format!("pruning generation {g}"), e)))?;
        klog::event!(Info, "kexec", "install.pruned", generation = g, sys_keep = keep.unwrap_or(0));
    }
    let n = p.number;
    st.create_generation(n).map_err(|e| failed(stick_err(&format!("making \\BITSOS2\\SYS\\{n}"), e)))?;
    // Anything wrong from here removes the new folder again (an unmarked complete folder would get a try).
    let undo = |what: String| -> InstallError {
        let gone = match st.remove_generation(n) {
            Ok(()) => format!("generation {n} removed again"),
            Err(e) => format!("generation {n} could NOT be removed again ({}): bitsboot gives it one try at most", errno_name(e)),
        };
        klog::event!(Error, "kexec", "install.failed", generation = n, error = what.as_str(), cleanup = gone.as_str());
        failed(format!("{what}; {gone}"))
    };
    // INITRD first: a cut while KERNEL is written leaves a KERNEL whose segments run past its end, which bitsboot
    // refuses at once (fallback without a try).
    for (name, data) in [("INITRD", initrd), ("KERNEL", kernel)] {
        st.write_file(n, name, data).map_err(|e| undo(stick_err(&format!("writing {name}"), e)))?;
    }
    for (name, data) in [("INITRD", initrd), ("KERNEL", kernel)] {
        let differs = verify(st, n, name, data).map_err(|e| undo(stick_err(&format!("reading {name} back"), e)))?;
        if let Some(t) = differs {
            return Err(undo(format!("{name} read back differs from what was written: {t}")));
        }
    }
    klog::event!(Info, "kexec", "install.verified", generation = n, kernel_bytes = kf.bytes, kernel_fnv = klog::Hex(kf.fnv),
        initrd_bytes = inf.bytes, initrd_fnv = klog::Hex(inf.fnv); "both files read back from the stick and equal");
    st.set_marker(n, Marker::New, true).map_err(|e| undo(stick_err("setting NEW", e)))?;
    for &(g, m) in &p.tidy {
        let marker = if m == Stale::Try { Marker::Try } else { Marker::New };
        if let Err(e) = st.set_marker(g, marker, false) {
            r.warnings.push(format!("a stale marker of the confirmed generation {g} stayed: {}", errno_name(e)));
        }
    }
    if let Some(c) = config {
        st.write_config(c).map_err(|e| failed(format!("generation {n} is installed (NEW), but CONFIG.TXT was not written: {}", errno_name(e))))?;
        r.config_written = true;
        klog::event!(Info, "kexec", "install.config_written", bytes = c.len());
    }
    r.next_boot = next_boot_text(st);
    klog::event!(Info, "kexec", "install.done", generation = n, replaced = p.replaced.unwrap_or(0), pruned = p.prune.len(),
        fallback = p.fallback.unwrap_or(0), next_boot = r.next_boot.as_str());
    Ok(r)
}

/// Bytes of one read-back piece.
const VERIFY_CHUNK: usize = 1 << 20;

/// Compares `name` of generation `n` as the stick holds it with `data`, one piece at a time: a kernel can be
/// hundreds of MiB, and a second whole copy next to the fetched one need not fit in one piece of memory.
/// None: equal; Some: how they differ. EFBIG if the stick's file is longer than `data`.
fn verify(st: &dyn Stick, n: u32, name: &str, data: &[u8]) -> Result<Option<String>, i64> {
    let mut buf = Vec::new();
    buf.try_reserve_exact(VERIFY_CHUNK).map_err(|_| uapi::errno::ENOMEM)?;
    buf.resize(VERIFY_CHUNK, 0);
    let (mut at, mut first, mut h) = (0usize, None, crate::Fnv::new());
    loop {
        let got = st.read_at(n, name, at as u64, &mut buf)?;
        if got == 0 {
            break;
        }
        if at + got > data.len() {
            return Err(uapi::errno::EFBIG);
        }
        let back = &buf[..got];
        h.update(back);
        if first.is_none() {
            first = back.iter().zip(&data[at..]).position(|(a, b)| a != b).map(|i| at + i);
        }
        at += got;
    }
    if first.is_none() && at == data.len() {
        return Ok(None);
    }
    Ok(Some(format!(
        "{at} bytes back of {}, first difference at byte {} (fnv {:#018x} vs {:#018x})",
        data.len(),
        first.unwrap_or(at),
        h.finish(),
        check::fnv(data)
    )))
}

/// "1 (OK), 3 (NEW)": the stick's generations for messages.
pub fn listing(st: &dyn Stick) -> String {
    match st.generations() {
        Ok(g) if g.is_empty() => String::from("no generation"),
        Ok(g) => g.iter().map(|g| format!("{} ({})", g.n, g.markers())).collect::<Vec<_>>().join(", "),
        Err(e) => format!("unknown ({})", errno_name(e)),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Confirmed {
    /// `OK` written, `TRY`/`NEW` cleared.
    Now { pruned: Vec<u32> },
    /// It had `OK` already.
    Already { pruned: Vec<u32> },
    /// The folder is gone (an install in this boot replaced it).
    Missing,
}

/// Confirms generation `n` on `st` (see `plan::confirm`). The caller holds `LOCK`.
pub fn confirm_on(st: &dyn Stick, n: u32) -> Result<Confirmed, String> {
    let gens = st.generations().map_err(|e| stick_err("listing \\BITSOS2\\SYS", e))?;
    let pin = st.pin().map_err(|e| stick_err("reading \\BITSOS2\\SYS\\PIN", e))?;
    let keep = st.config().ok().and_then(|c| check::sys_keep(c.as_deref()).ok());
    let plan = plan::confirm(&gens, n, pin, keep.unwrap_or(1));
    let (now, mut prune, tidy) = match plan {
        ConfirmPlan::Missing => return Ok(Confirmed::Missing),
        ConfirmPlan::AlreadyOk { prune, tidy } => (false, prune, tidy),
        ConfirmPlan::Confirm { prune, tidy } => (true, prune, tidy),
    };
    if keep.is_none() {
        prune.clear();
    }
    if now {
        st.set_marker(n, Marker::Ok, true).map_err(|e| stick_err("writing OK", e))?;
    }
    // OK beats TRY beats NEW: each step below leaves a confirmed generation.
    for m in [Marker::Try, Marker::New] {
        st.set_marker(n, m, false).map_err(|e| stick_err("clearing TRY/NEW", e))?;
    }
    for (g, m) in tidy {
        let _ = st.set_marker(g, if m == Stale::Try { Marker::Try } else { Marker::New }, false);
    }
    let mut pruned = Vec::new();
    for g in prune {
        if st.set_marker(g, Marker::Ok, false).and_then(|()| st.remove_generation(g)).is_ok() {
            pruned.push(g);
        }
    }
    Ok(if now { Confirmed::Now { pruned } } else { Confirmed::Already { pruned } })
}

/// One line for `reload_info`: the stick's generations and what bitsboot starts at the next boot (read only).
pub fn stick_summary() -> String {
    let Some(vol) = fs_fat::stick::volume() else {
        return format!("not mounted ({})", fs_fat::stick::last_reason().unwrap_or("none recognised"));
    };
    let rw = if vol.writable() { "read-write" } else { "read-only" };
    match fs_fat::stick::with_sys(|s| {
        let st = FatStick(s);
        format!("{rw}; generations {}; next boot starts {}", listing(&st), next_boot_text(&st))
    }) {
        Ok(t) => t,
        Err(e) => format!("{rw}; {}", errno_name(e)),
    }
}
