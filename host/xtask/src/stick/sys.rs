//! System generations `\BITSOS2\SYS\<n>\` on the stick (V2-PLAN §7, B1): reading them, planning a bundle,
//! a confirmation and the pruning as a list of steps, and carrying the steps out. Each generation is written
//! once, into `SYS\STAGING` first (bitsboot ignores names that are not numbers), and appears under its number
//! by one rename after its files and marker are on the stick.
use super::bundle::Bundle;
use super::gate::{find_ci, read_bounded, resolve_ci};
use std::fs::File;
use std::io::Write;
use std::path::Path;

pub const SYS: &str = "BITSOS2/SYS";
pub const STAGING: &str = "STAGING";
pub const SYS_KEEP_DEFAULT: u32 = 5;

/// bitsboot `gens::parse_name`: decimal digits only, fits in u32.
pub fn parse_name(name: &str) -> Option<u32> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    name.parse().ok()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Gen {
    pub number: u32,
    /// The directory's name on the stick.
    pub name: String,
    /// File names on the stick, as stored.
    pub files: Vec<String>,
    pub kernel: bool,
    pub initrd: bool,
    pub new: bool,
    pub tried: bool,
    pub ok: bool,
}

impl Gen {
    pub fn file(&self, upper: &str) -> Option<&str> {
        self.files.iter().find(|f| f.eq_ignore_ascii_case(upper)).map(|s| s.as_str())
    }

    pub fn markers(&self) -> String {
        let on: Vec<&str> = [(self.new, "NEW"), (self.tried, "TRY"), (self.ok, "OK")].iter().filter(|(b, _)| *b).map(|(_, n)| *n).collect();
        let m = if on.is_empty() { "no marker".to_string() } else { on.join("+") };
        let miss: Vec<&str> = [(self.kernel, "KERNEL"), (self.initrd, "INITRD")].iter().filter(|(b, _)| !*b).map(|(_, n)| *n).collect();
        if miss.is_empty() { m } else { format!("{m}, missing {}", miss.join("+")) }
    }

}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Pin {
    #[default]
    None,
    Number(u32),
    Invalid,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SysState {
    /// Sorted by number.
    pub gens: Vec<Gen>,
    pub pin: Pin,
    /// The PIN file's name on the stick.
    pub pin_name: Option<String>,
    pub staging: Option<String>,
    /// Other entries below SYS (left alone, named in the plan).
    pub junk: Vec<String>,
    /// Numbers that two directories share (`1` and `01`).
    pub dupes: Vec<u32>,
    pub bitsos2: Option<String>,
    pub sys: Option<String>,
}

impl SysState {
    pub fn newest(&self) -> Option<&Gen> {
        self.gens.last()
    }

    pub fn summary(&self) -> String {
        if self.gens.is_empty() {
            return "no system generation yet".into();
        }
        self.gens.iter().map(|g| format!("{} {}", g.number, g.markers())).collect::<Vec<_>>().join(", ")
    }
}

pub fn scan(point: &Path) -> Result<SysState, String> {
    let mut st = SysState::default();
    let Some(b2) = find_ci(point, "BITSOS2") else { return Ok(st) };
    st.bitsos2 = b2.file_name().map(|n| n.to_string_lossy().to_string());
    let Some(sys) = find_ci(&b2, "SYS") else { return Ok(st) };
    st.sys = sys.file_name().map(|n| n.to_string_lossy().to_string());
    let rd = std::fs::read_dir(&sys).map_err(|e| format!("{}: {e}", sys.display()))?;
    for e in rd {
        let e = e.map_err(|e| format!("{}: {e}", sys.display()))?;
        let name = e.file_name().to_string_lossy().to_string();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir && let Some(number) = parse_name(&name) {
            if st.gens.iter().any(|g| g.number == number) {
                st.dupes.push(number);
                continue;
            }
            let mut g = Gen { number, name: name.clone(), ..Gen::default() };
            for f in std::fs::read_dir(e.path()).map_err(|x| format!("{}: {x}", e.path().display()))?.flatten() {
                if f.file_type().map(|t| t.is_dir()).unwrap_or(true) {
                    continue;
                }
                let n = f.file_name().to_string_lossy().to_string();
                let is = |s: &str| n.eq_ignore_ascii_case(s);
                g.kernel |= is("KERNEL");
                g.initrd |= is("INITRD");
                g.new |= is("NEW");
                g.tried |= is("TRY");
                g.ok |= is("OK");
                g.files.push(n);
            }
            g.files.sort();
            st.gens.push(g);
        } else if is_dir && name.eq_ignore_ascii_case(STAGING) {
            st.staging = Some(name);
        } else if !is_dir && name.eq_ignore_ascii_case("PIN") {
            let text = read_bounded(&e.path(), 4096).unwrap_or_default();
            st.pin = std::str::from_utf8(&text).ok().and_then(|t| parse_name(t.trim())).map(Pin::Number).unwrap_or(Pin::Invalid);
            st.pin_name = Some(name);
        } else {
            st.junk.push(name);
        }
    }
    st.gens.sort_by_key(|g| g.number);
    st.junk.sort();
    Ok(st)
}

/// Bound for reading CONFIG.TXT (a few hundred bytes in practice).
const MAX_CONFIG: u64 = 1 << 20;

/// `sys_keep=` from `\BITSOS2\CONFIG.TXT` (last line wins, `#` comments), with what to tell about it. The
/// pruning deletes by this number, so a CONFIG.TXT that exists but cannot be read, or a sys_keep that is no
/// number, refuses instead of falling back to the default (Georg may have asked to keep more).
pub fn sys_keep(point: &Path) -> Result<(u32, String), String> {
    let Some(f) = resolve_ci(point, "BITSOS2/CONFIG.TXT") else { return Ok((SYS_KEEP_DEFAULT, "default, no CONFIG.TXT".into())) };
    let unknown = |why: String| format!("refused: \\BITSOS2\\CONFIG.TXT: {why}; the number of generations to keep is unknown and nothing is pruned on a guess (give --sys-keep <n>). Nothing was written.");
    let text = read_bounded(&f, MAX_CONFIG).map_err(unknown)?;
    let mut found = None;
    for raw in text.split(|&b| b == b'\n') {
        let l = String::from_utf8_lossy(raw);
        let l = l.split('#').next().unwrap_or("").trim().to_string();
        if let Some((k, v)) = l.split_once('=')
            && k.trim() == "sys_keep"
        {
            found = Some(v.trim().to_string());
        }
    }
    match found.as_deref().map(|v| (v, v.parse::<u32>())) {
        None => Ok((SYS_KEEP_DEFAULT, "default, CONFIG.TXT sets none".into())),
        Some((_, Ok(0))) => Ok((1, "CONFIG.TXT says 0; at least the newest confirmed one stays".into())),
        Some((_, Ok(n))) => Ok((n, "CONFIG.TXT".into())),
        Some((v, Err(_))) => Err(unknown(format!("sys_keep={v} is no number"))),
    }
}

/// One change on the stick; paths are `/`-separated below the mount point, names as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Mkdir(String),
    Write { to: String, bytes: Vec<u8>, from: Option<String> },
    Remove(String),
    RemoveTree(String),
    Rename { from: String, to: String },
    Sync,
}

pub fn dos(rel: &str) -> String {
    format!("\\{}", rel.replace('/', "\\"))
}

fn size(n: usize) -> String {
    if n >= 1 << 20 { format!("{:.1} MiB", n as f64 / (1 << 20) as f64) } else { format!("{n} bytes") }
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Step::Mkdir(p) => write!(f, "make folder {}", dos(p)),
            Step::Write { to, bytes, from: Some(src) } => write!(f, "write {} ({}, from {src})", dos(to), size(bytes.len())),
            Step::Write { to, bytes, from: None } if bytes.is_empty() => write!(f, "write {} (empty marker)", dos(to)),
            Step::Write { to, bytes, from: None } => write!(f, "write {} ({})", dos(to), size(bytes.len())),
            Step::Remove(p) => write!(f, "delete {}", dos(p)),
            Step::RemoveTree(p) => write!(f, "delete folder {} with everything in it", dos(p)),
            Step::Rename { from, to } => write!(f, "rename {} to {}", dos(from), dos(to)),
            Step::Sync => write!(f, "flush the stick (syncfs)"),
        }
    }
}

/// The only places the tool changes (R8b): below `\BITSOS2\`, and `\EFI\BOOT\` for the loader.
pub fn allowed(step: &Step) -> bool {
    let ok = |p: &str| {
        let parts: Vec<&str> = p.split('/').collect();
        if parts.iter().any(|c| c.is_empty() || *c == "." || *c == ".." || c.contains('\\')) {
            return false;
        }
        let ci = |i: usize, s: &str| parts.get(i).is_some_and(|c| c.eq_ignore_ascii_case(s));
        ci(0, "BITSOS2") || (ci(0, "EFI") && (parts.len() == 1 || (ci(1, "BOOT") && parts.len() <= 3)))
    };
    let sys_tree = |p: &str| {
        let parts: Vec<&str> = p.split('/').collect();
        ok(p) && parts.len() == 3 && parts[0].eq_ignore_ascii_case("BITSOS2") && parts[1].eq_ignore_ascii_case("SYS")
    };
    match step {
        Step::Mkdir(p) | Step::Write { to: p, .. } | Step::Remove(p) => ok(p),
        Step::RemoveTree(p) => sys_tree(p),
        Step::Rename { from, to } => ok(from) && ok(to),
        Step::Sync => true,
    }
}

/// Builds the step list on a model of the stick, so combined operations (confirm, then a bundle) see each
/// other's result.
pub struct Planner<'a> {
    pub point: &'a Path,
    pub st: SysState,
    pub steps: Vec<Step>,
    pub notes: Vec<String>,
}

impl<'a> Planner<'a> {
    pub fn new(point: &'a Path, st: SysState) -> Result<Planner<'a>, String> {
        if !st.dupes.is_empty() {
            return Err(format!("refused: {} has two folders for generation(s) {:?} (e.g. 1 and 01); sort that out by hand first", dos(SYS), st.dupes));
        }
        Ok(Planner { point, st, steps: Vec::new(), notes: Vec::new() })
    }

    fn sys_rel(&self) -> String {
        format!("{}/{}", self.st.bitsos2.as_deref().unwrap_or("BITSOS2"), self.st.sys.as_deref().unwrap_or("SYS"))
    }

    fn ensure_sys(&mut self) {
        if self.st.bitsos2.is_none() {
            self.steps.push(Step::Mkdir("BITSOS2".into()));
            self.st.bitsos2 = Some("BITSOS2".into());
        }
        if self.st.sys.is_none() {
            self.steps.push(Step::Mkdir(self.sys_rel()));
            self.st.sys = Some("SYS".into());
        }
    }

    fn gen_rel(&self, g: &Gen) -> String {
        format!("{}/{}", self.sys_rel(), g.name)
    }

    pub fn unpin(&mut self) {
        match self.st.pin_name.take() {
            Some(n) => {
                self.steps.push(Step::Remove(format!("{}/{n}", self.sys_rel())));
                self.notes.push(format!("the pin ({:?}) is removed: bitsboot starts by the marker rule again", self.st.pin));
                self.st.pin = Pin::None;
            }
            None => self.notes.push("no PIN on the stick, nothing to unpin".into()),
        }
    }

    /// Confirms the newest generation after a green round on it: `OK` is written, then `TRY` (and `NEW`) go.
    pub fn confirm(&mut self, n: u32) -> Result<(), String> {
        let Some(g) = self.st.newest().cloned() else { return Err("refused: --confirm: the stick has no generation".into()) };
        if g.number != n {
            return Err(format!("refused: --confirm {n}: the newest generation is {} (only the newest one is ever under trial)", g.number));
        }
        if g.ok {
            self.notes.push(format!("generation {n} is already confirmed (OK)"));
            return Ok(());
        }
        if !g.tried {
            return Err(format!("refused: --confirm {n}: generation {n} was never started (no TRY next to it); a Reload-proven kernel is installed with --proven"));
        }
        if !g.kernel || !g.initrd {
            return Err(format!("refused: --confirm {n}: generation {n} lacks its KERNEL or INITRD"));
        }
        let d = self.gen_rel(&g);
        self.steps.push(Step::Write { to: format!("{d}/OK"), bytes: Vec::new(), from: None });
        self.steps.push(Step::Sync);
        for m in ["TRY", "NEW"] {
            if let Some(f) = g.file(m) {
                self.steps.push(Step::Remove(format!("{d}/{f}")));
            }
        }
        let gm = self.st.gens.last_mut().expect("checked above");
        gm.files.retain(|f| !f.eq_ignore_ascii_case("TRY") && !f.eq_ignore_ascii_case("NEW"));
        gm.files.push("OK".into());
        (gm.ok, gm.tried, gm.new) = (true, false, false);
        Ok(())
    }

    fn same_as(&self, g: &Gen, b: &Bundle) -> bool {
        let read = |n: &str| g.file(n).and_then(|f| std::fs::read(self.point.join(self.gen_rel(g)).join(f)).ok());
        read("KERNEL").as_deref() == Some(&b.kernel[..]) && read("INITRD").as_deref() == Some(&b.initrd[..])
    }

    /// A bundle as a new generation (or in place of an unconfirmed newest one); `proven` = it passed a Mini-PC
    /// Reload round, so it goes on as `OK`. `loader` = the stick's current `BOOTX64.EFI`.
    pub fn bundle(&mut self, b: &Bundle, proven: bool, loader: Option<&[u8]>, force: bool) -> Result<u32, String> {
        let newest = self.st.newest().cloned();
        if let Some(g) = &newest
            && !force
            && self.same_as(g, b)
        {
            return Err(format!("refused: KERNEL and INITRD are the same as generation {}'s; nothing to write (--force writes them anyway)", g.number));
        }
        let replace = newest.as_ref().filter(|g| !g.ok).cloned();
        let number = match (&newest, &replace) {
            (_, Some(g)) => g.number,
            (Some(g), None) => g.number.checked_add(1).ok_or("refused: the newest generation number is already the largest possible")?,
            (None, None) => 1,
        };
        if loader.is_none() && b.bitsboot.is_none() {
            return Err("refused: the stick has no \\EFI\\BOOT\\BOOTX64.EFI and the bundle brings no bitsboot; it would not start".into());
        }
        self.ensure_sys();
        let sys = self.sys_rel();
        if let Some(s) = self.st.staging.take() {
            self.steps.push(Step::RemoveTree(format!("{sys}/{s}")));
            self.notes.push(format!("{} is left over from an interrupted write and goes first", dos(&format!("{sys}/{s}"))));
        }
        let stage = format!("{sys}/{STAGING}");
        let src = |p: &Path| Some(p.display().to_string());
        self.steps.push(Step::Mkdir(stage.clone()));
        self.steps.push(Step::Write { to: format!("{stage}/KERNEL"), bytes: b.kernel.clone(), from: src(&b.kernel_from) });
        self.steps.push(Step::Write { to: format!("{stage}/INITRD"), bytes: b.initrd.clone(), from: src(&b.initrd_from) });
        let marker = if proven { "OK" } else { "NEW" };
        self.steps.push(Step::Write { to: format!("{stage}/{marker}"), bytes: Vec::new(), from: None });
        self.steps.push(Step::Sync);
        match (&b.bitsboot, loader) {
            (Some((nb, _)), Some(old)) if nb[..] == *old => self.notes.push("bitsboot is unchanged and stays".into()),
            (Some((nb, from)), _) => {
                let efi = resolve_ci(self.point, "EFI").and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()));
                let efi = efi.unwrap_or_else(|| {
                    self.steps.push(Step::Mkdir("EFI".into()));
                    "EFI".into()
                });
                let boot = find_ci(&self.point.join(&efi), "BOOT").and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()));
                let boot = format!(
                    "{efi}/{}",
                    boot.unwrap_or_else(|| {
                        self.steps.push(Step::Mkdir(format!("{efi}/BOOT")));
                        "BOOT".into()
                    })
                );
                let cur = find_ci(&self.point.join(&boot), "BOOTX64.EFI").and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()));
                self.steps.push(Step::Write { to: format!("{boot}/BOOTX64.NEW"), bytes: nb.clone(), from: src(from) });
                self.steps.push(Step::Sync);
                self.steps.push(Step::Rename { from: format!("{boot}/BOOTX64.NEW"), to: format!("{boot}/{}", cur.unwrap_or_else(|| "BOOTX64.EFI".into())) });
            }
            (None, _) => {}
        }
        if let Some(g) = &replace {
            self.steps.push(Step::RemoveTree(self.gen_rel(g)));
            self.notes.push(format!("generation {} ({}) was never confirmed: the bundle takes its place", g.number, g.markers()));
            self.st.gens.pop();
        }
        self.steps.push(Step::Rename { from: stage, to: format!("{sys}/{number}") });
        self.steps.push(Step::Sync);
        let files = vec!["INITRD".to_string(), "KERNEL".to_string(), marker.to_string()];
        self.st.gens.push(Gen { number, name: number.to_string(), files, kernel: true, initrd: true, new: !proven, tried: false, ok: proven });
        match (&self.st.pin, proven) {
            (Pin::None, _) => {}
            (_, true) => {
                self.notes.push("a confirmed install ends the pin (V2-PLAN §7)".into());
                self.unpin();
            }
            (Pin::Number(p), false) if replace.as_ref().is_some_and(|g| g.number == *p) => {
                self.notes.push(format!("the PIN named the replaced, unconfirmed generation {p}"));
                self.unpin();
            }
            (Pin::Number(p), false) if self.st.gens.iter().any(|g| g.number == *p && g.ok) => self.notes.push(format!(
                "WARNING: PIN names generation {p}: bitsboot keeps starting it, and generation {number} waits until the pin goes (--unpin)"
            )),
            (_, false) => self.notes.push("the PIN names no confirmed generation; bitsboot ignores it".into()),
        }
        Ok(number)
    }

    /// `\BITSOS2\CONFIG.TXT` from `text`, only when the stick has none (the stick's own is Georg's).
    pub fn config(&mut self, text: &[u8], from: &Path) {
        if resolve_ci(self.point, "BITSOS2/CONFIG.TXT").is_some() {
            self.notes.push("the stick's own \\BITSOS2\\CONFIG.TXT stays as it is (--config is only for a stick without one)".into());
            return;
        }
        self.ensure_sys();
        let b2 = self.st.bitsos2.clone().unwrap_or_else(|| "BITSOS2".into());
        self.steps.push(Step::Write { to: format!("{b2}/CONFIG.TXT"), bytes: text.to_vec(), from: Some(from.display().to_string()) });
    }

    /// Stale markers next to `OK` (a confirmation cut short) go, then generations beyond `keep` confirmed ones.
    pub fn finish(&mut self, keep: u32) {
        if let Some(s) = self.st.staging.take() {
            let p = format!("{}/{s}", self.sys_rel());
            self.notes.push(format!("{} is left over from an interrupted write and goes", dos(&p)));
            self.steps.push(Step::RemoveTree(p));
        }
        for i in 0..self.st.gens.len() {
            let g = self.st.gens[i].clone();
            if !g.ok {
                continue;
            }
            for m in ["TRY", "NEW"] {
                if let Some(f) = g.file(m) {
                    self.steps.push(Step::Remove(format!("{}/{f}", self.gen_rel(&g))));
                    self.notes.push(format!("generation {} is confirmed; its stale {m} goes", g.number));
                }
            }
            let gm = &mut self.st.gens[i];
            gm.files.retain(|f| !f.eq_ignore_ascii_case("TRY") && !f.eq_ignore_ascii_case("NEW"));
            (gm.tried, gm.new) = (false, false);
        }
        let pin = match self.st.pin {
            Pin::Number(p) if self.st.pin_name.is_some() => Some(p),
            _ => None,
        };
        for n in prune(&self.st.gens, keep, pin) {
            let g = self.st.gens.iter().find(|g| g.number == n).cloned().expect("from the list");
            if let Some(f) = g.file("OK") {
                self.steps.push(Step::Remove(format!("{}/{f}", self.gen_rel(&g))));
            }
            self.steps.push(Step::RemoveTree(self.gen_rel(&g)));
            self.notes.push(format!("generation {n} ({}) is pruned", g.markers()));
            self.st.gens.retain(|x| x.number != n);
        }
        for j in &self.st.junk {
            self.notes.push(format!("{}\\{j} is not a generation and stays", dos(&self.sys_rel())));
        }
        // A loader update cut between writing BOOTX64.NEW and the rename leaves the new file behind; unless
        // this plan writes it again, it goes.
        let loader_planned = self.steps.iter().any(|s| matches!(s, Step::Write { to, .. } if to.to_ascii_uppercase().ends_with("/BOOTX64.NEW")));
        if !loader_planned
            && let Some(f) = resolve_ci(self.point, "EFI/BOOT/BOOTX64.NEW").filter(|f| f.is_file())
            && let Ok(rel) = f.strip_prefix(self.point)
        {
            let rel = rel.to_string_lossy().to_string();
            self.notes.push(format!("{} is left over from an interrupted loader update and goes", dos(&rel)));
            self.steps.push(Step::Remove(rel));
        }
        if !self.steps.is_empty() && self.steps.last() != Some(&Step::Sync) {
            self.steps.push(Step::Sync);
        }
    }
}

/// The generations to delete: everything but the newest one (confirmed or under trial), the newest `keep`
/// confirmed ones and the pinned one. The newest confirmed one is always among the kept (keep >= 1).
pub fn prune(gens: &[Gen], keep: u32, pin: Option<u32>) -> Vec<u32> {
    let keep = keep.max(1) as usize;
    let newest = gens.iter().map(|g| g.number).max();
    let mut oks: Vec<u32> = gens.iter().filter(|g| g.ok).map(|g| g.number).collect();
    oks.sort_unstable_by(|a, b| b.cmp(a));
    let kept: Vec<u32> = oks.into_iter().take(keep).chain(newest).chain(pin).collect();
    let mut out: Vec<u32> = gens.iter().map(|g| g.number).filter(|n| !kept.contains(n)).collect();
    out.sort_unstable();
    out
}

pub fn sync_fs(point: &Path) -> Result<(), String> {
    let st = std::process::Command::new("sync").arg("-f").arg(point).status().map_err(|e| format!("sync: {e}"))?;
    if st.success() { Ok(()) } else { Err(format!("sync -f {}: {st}", point.display())) }
}

fn sync_dir(p: &Path) {
    // Best effort: vfat flushes directories with syncfs anyway, which every plan ends with.
    if let Ok(d) = File::open(p) {
        let _ = d.sync_all();
    }
}

#[cfg(test)]
thread_local! {
    /// Tests: how many more file operations succeed before `checkpoint` fails (a cut-off write).
    pub static FAIL_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Called before every change the tool makes (stick and backup); a test makes it fail after N operations
/// to check that every cut leaves a stick that starts and a backup that is never trusted half-done.
pub fn checkpoint(what: &str) -> Result<(), String> {
    #[cfg(test)]
    FAIL_AFTER.with(|c| match c.get() {
        Some(0) => Err(format!("injected fault before {what}")),
        Some(n) => {
            c.set(Some(n - 1));
            Ok(())
        }
        None => Ok(()),
    })?;
    let _ = what;
    Ok(())
}

/// Carries out the steps in order, each write flushed and read back. Stops at the first error.
pub fn apply(point: &Path, steps: &[Step]) -> Result<(), String> {
    if let Some(bad) = steps.iter().find(|s| !allowed(s)) {
        return Err(format!("internal refusal: '{bad}' lies outside \\BITSOS2\\ and \\EFI\\BOOT\\; nothing was written"));
    }
    for (i, s) in steps.iter().enumerate() {
        let fail = |e: String| format!("step {} ({s}) failed: {e}; the stick holds the steps before it", i + 1);
        checkpoint(&s.to_string()).map_err(fail)?;
        match s {
            Step::Mkdir(p) => match std::fs::create_dir(point.join(p)) {
                Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(fail(e.to_string())),
                _ => {}
            },
            Step::Write { to, bytes, .. } => {
                let p = point.join(to);
                let mut f = File::create(&p).map_err(|e| fail(e.to_string()))?;
                f.write_all(bytes).and_then(|_| f.sync_all()).map_err(|e| fail(e.to_string()))?;
                drop(f);
                let back = std::fs::read(&p).map_err(|e| fail(e.to_string()))?;
                if back != *bytes {
                    return Err(fail("the read-back differs from what was written".into()));
                }
            }
            Step::Remove(p) => std::fs::remove_file(point.join(p)).map_err(|e| fail(e.to_string()))?,
            Step::RemoveTree(p) => std::fs::remove_dir_all(point.join(p)).map_err(|e| fail(e.to_string()))?,
            Step::Rename { from, to } => {
                let t = point.join(to);
                std::fs::rename(point.join(from), &t).map_err(|e| fail(e.to_string()))?;
                if let Some(parent) = t.parent() {
                    sync_dir(parent);
                }
            }
            Step::Sync => sync_fs(point).map_err(fail)?,
        }
        println!("  done {:>2}. {s}", i + 1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(n: u32, ok: bool) -> Gen {
        Gen { number: n, name: n.to_string(), ok, new: !ok, kernel: true, initrd: true, ..Gen::default() }
    }

    #[test]
    fn prune_keeps_newest_pin_and_keep_confirmed() {
        let gens: Vec<Gen> = (1..=7).map(|n| g(n, n != 7)).collect();
        assert_eq!(prune(&gens, 2, None), vec![1, 2, 3, 4]);
        assert_eq!(prune(&gens, 2, Some(1)), vec![2, 3, 4]);
        assert_eq!(prune(&gens, 0, None), vec![1, 2, 3, 4, 5]); // 0 counts as 1: 6 (newest OK) and 7 stay
        assert_eq!(prune(&gens, 50, None), Vec::<u32>::new());
        assert_eq!(prune(&[], 5, Some(3)), Vec::<u32>::new());
        // An older unconfirmed generation is never a fallback and goes.
        let gens = vec![g(1, true), g(2, false), g(3, true)];
        assert_eq!(prune(&gens, 5, None), vec![2]);
    }

    #[test]
    fn prune_invariants_over_many_layouts() {
        // Every OK/unconfirmed pattern of up to 8 generations, keep 1..4, every pin.
        for len in 0..=8u32 {
            for bits in 0..(1u32 << len) {
                let gens: Vec<Gen> = (0..len).map(|i| g(i + 1, bits & (1 << i) != 0)).collect();
                for keep in 1..=4 {
                    for pin in std::iter::once(None).chain((1..=len + 1).map(Some)) {
                        let gone = prune(&gens, keep, pin);
                        let left: Vec<&Gen> = gens.iter().filter(|x| !gone.contains(&x.number)).collect();
                        let newest_ok = gens.iter().filter(|x| x.ok).map(|x| x.number).max();
                        assert!(newest_ok.is_none_or(|n| !gone.contains(&n)));
                        assert!(len == 0 || !gone.contains(&len), "newest stays");
                        assert!(pin.is_none_or(|p| !gone.contains(&p)), "pin stays");
                        let ok_left = left.iter().filter(|x| x.ok).count();
                        let ok_all = gens.iter().filter(|x| x.ok).count();
                        assert!(ok_left >= ok_all.min(keep as usize));
                        let extra = left.iter().filter(|x| x.ok && Some(x.number) != pin).count();
                        assert!(extra <= keep as usize, "{bits:b} keep {keep} pin {pin:?}");
                        assert!(left.iter().all(|x| x.ok || x.number == len || Some(x.number) == pin));
                    }
                }
            }
        }
    }

    #[test]
    fn steps_stay_in_their_places() {
        let w = |p: &str| Step::Write { to: p.into(), bytes: vec![], from: None };
        for p in ["BITSOS2/SYS/1/OK", "bitsos2/CONFIG.TXT", "EFI/BOOT/BOOTX64.NEW", "EFI", "EFI/BOOT", "BITSOS2"] {
            assert!(allowed(&w(p)), "{p}");
        }
        for p in ["BITSOS/STICK.ID", "EFI/BOOT/X/Y", "EFI/MICROSOFT/BOOT", "BITSOS2/../BITSOS/X", "", "/BITSOS2/X", "BITSOS2//X", "BITSOS2/a\\b", "Windows"] {
            assert!(!allowed(&w(p)), "{p}");
        }
        assert!(allowed(&Step::RemoveTree("BITSOS2/SYS/3".into())));
        for p in ["BITSOS2/SYS", "BITSOS2", "BITSOS2/SYS/3/x", "EFI/BOOT/BOOTX64.EFI", "BITSOS"] {
            assert!(!allowed(&Step::RemoveTree(p.into())), "{p}");
        }
        assert!(!allowed(&Step::Rename { from: "BITSOS2/SYS/STAGING".into(), to: "BITSOS/SYS".into() }));
    }

    #[test]
    fn names() {
        assert_eq!(parse_name("12"), Some(12));
        assert_eq!(parse_name("4294967295"), Some(u32::MAX));
        for bad in ["", "99999999999", "-1", "1a", "STAGING", " 1"] {
            assert_eq!(parse_name(bad), None);
        }
    }
}
