//! `xtask stick` (BT-3): puts BitsOS V2 onto Georg's boot stick on the Gaming-PC, through its mounted FAT32
//! file system only (never a raw device, never mkfs, never a partition change). The target must be mounted
//! vfat FAT32, labelled BITSOS, with a STICK.ID equal to the recorded one; anything else is refused, the label
//! even with --force. A dry run is the default; --write shows the same plan and waits for a typed J, then
//! checks the stick again. V2-PLAN §7 and §7.1 (generations, markers, plug-over), V2-LANES BT-3.
mod bundle;
mod gate;
mod retire;
mod sys;
#[cfg(test)]
mod tests;

use crate::util::*;
use gate::{Ident, Loader, Probe};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

pub const HELP: &str = "usage: cargo xtask stick --target <mountpoint> [--write] [operation]
  Without --write nothing is written (dry run): the stick, its generations and the plan are shown.
  With --write the plan is shown, then the tool waits for J and Enter.
  --id <32 hex> | --id-file <file>   the recorded stick ID (default: <repo>/stick.id)
  --force                           accept a stick on another bus than USB, or a bundle equal to the newest
                                    generation; never a missing label, a wrong ID or a built-in disk
operations (a bundle, --confirm, --unpin and --config combine; --prune runs with every write anyway):
  --retire-v1 [--backup <dir>]      once: copy the stick to <dir> (default <repo>/stick-backup-v1-<date>),
                                    compare, move STICK.ID to \\BITSOS2\\, remove \\BITSOS\\ and V1's loader
  --bundle <dir>                    <dir>/KERNEL, <dir>/INITRD (+ <dir>/BOOTX64.EFI) as a new generation
  --kernel <f> --initrd <f> [--bitsboot <f>]   the same from single files
  --build [--release]               build kernel, initramfs and bitsboot first, then write them as a bundle
  --proven                          the bundle passed a Mini-PC Reload round: install it as OK (ends a pin)
  --config <file>                   \\BITSOS2\\CONFIG.TXT, only on a stick without one
  --confirm <n>                     generation <n> (the newest, with TRY) had a green round: OK, TRY goes
  --unpin                           remove \\BITSOS2\\SYS\\PIN
  --prune [--sys-keep <n>]          keep the newest <n> confirmed generations (CONFIG.TXT sys_keep, default 5)";

pub struct Opts {
    pub target: PathBuf,
    pub write: bool,
    pub force: bool,
    pub recorded: Option<[u8; 16]>,
    pub record_src: String,
    pub retire: bool,
    pub backup: PathBuf,
    pub bundle: Option<bundle::Sources>,
    pub build: Option<bool>,
    pub proven: bool,
    pub config: Option<PathBuf>,
    pub confirm: Option<u32>,
    pub prune: bool,
    pub unpin: bool,
    pub sys_keep: Option<u32>,
    /// Where the per-stick lock file lives (outside the stick): $XDG_RUNTIME_DIR, else the temp folder.
    pub lock_dir: PathBuf,
}

impl Opts {
    pub fn parse(root: &Path, a: &mut Args) -> Result<Opts, String> {
        let write = a.flag("--write");
        if a.flag("--dry-run") && write {
            return Err("stick: --dry-run and --write exclude each other".into());
        }
        let target = a.value("--target")?.ok_or_else(|| format!("stick needs --target <mountpoint>\n{HELP}"))?;
        let (id, id_file) = (a.value("--id")?, a.value("--id-file")?);
        let (recorded, record_src) = match (id, id_file) {
            (Some(_), Some(_)) => return Err("stick: give --id or --id-file, not both".into()),
            (Some(h), None) => (Some(gate::parse_id(h.as_bytes()).ok_or("stick: --id needs 32 hex digits")?), "--id".to_string()),
            (None, f) => {
                let f = f.map(PathBuf::from).unwrap_or_else(|| root.join("stick.id"));
                match std::fs::read(&f) {
                    Ok(b) => (Some(gate::parse_id(&b).ok_or_else(|| format!("stick: {} holds no stick ID (32 hex digits)", f.display()))?), f.display().to_string()),
                    Err(_) => (None, format!("{} (missing)", f.display())),
                }
            }
        };
        let retire = a.flag("--retire-v1");
        let backup = a.value("--backup")?;
        if backup.is_some() && !retire {
            return Err("stick: --backup belongs to --retire-v1".into());
        }
        let backup = backup.map(PathBuf::from).unwrap_or_else(|| root.join(format!("stick-backup-v1-{}", retire::today())));
        let (dir, kernel, initrd, bitsboot) = (a.value("--bundle")?, a.value("--kernel")?, a.value("--initrd")?, a.value("--bitsboot")?);
        let build = a.flag("--build").then(|| a.flag("--release"));
        let bundle = match (dir, kernel, initrd) {
            (Some(d), None, None) if bitsboot.is_none() && build.is_none() => Some(bundle::Sources::from_dir(Path::new(&d))),
            (None, Some(k), Some(i)) if build.is_none() => Some(bundle::Sources { kernel: k.into(), initrd: i.into(), bitsboot: bitsboot.map(PathBuf::from) }),
            (None, None, None) if bitsboot.is_none() => None,
            _ => return Err("stick: a bundle is --bundle <dir>, or --kernel and --initrd (+ --bitsboot), or --build".into()),
        };
        let o = Opts {
            target: target.into(),
            write,
            force: a.flag("--force"),
            recorded,
            record_src,
            retire,
            backup,
            bundle,
            build,
            proven: a.flag("--proven"),
            config: a.value("--config")?.map(PathBuf::from),
            confirm: a.num("--confirm")?,
            prune: a.flag("--prune"),
            unpin: a.flag("--unpin"),
            sys_keep: a.num("--sys-keep")?,
            lock_dir: std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|d| d.is_dir()).unwrap_or_else(std::env::temp_dir),
        };
        if !a.positionals()?.is_empty() {
            return Err(format!("stick: unexpected arguments {:?}", a.args));
        }
        if o.proven && o.bundle.is_none() && o.build.is_none() {
            return Err("stick: --proven belongs to a bundle".into());
        }
        if o.retire && o.has_sys_ops() {
            return Err("stick: --retire-v1 runs alone (write the first bundle afterwards)".into());
        }
        Ok(o)
    }

    fn has_sys_ops(&self) -> bool {
        self.bundle.is_some() || self.build.is_some() || self.confirm.is_some() || self.prune || self.unpin || self.config.is_some() || self.sys_keep.is_some()
    }
}

pub fn cmd_stick(root: &Path, mut a: Args) -> Result<(), String> {
    let Some(o) = prepare(root, &mut a, &mut build_bundle)? else { return Ok(()) };
    run(&gate::HostProbe, &o, &mut |prompt: &str| {
        print!("{prompt}");
        let _ = std::io::stdout().flush();
        let mut l = String::new();
        let _ = std::io::stdin().lock().read_line(&mut l);
        l
    })
}

/// A build step: (repo root, release) -> the built bundle's files. Tests put a fake in.
pub type BuildFn<'a> = dyn FnMut(&Path, bool) -> Result<bundle::Sources, String> + 'a;

/// The options, with `--build` carried out (None after --help).
pub fn prepare(root: &Path, a: &mut Args, build: &mut BuildFn) -> Result<Option<Opts>, String> {
    if a.flag("--help") || a.flag("-h") {
        println!("{HELP}");
        return Ok(None);
    }
    let mut o = Opts::parse(root, a)?;
    if let Some(release) = o.build {
        o.bundle = Some(build(root, release)?);
    }
    Ok(Some(o))
}

/// Kernel, initramfs and bitsboot as `xtask image` builds them (no ktest feature), in `target/xtask/stick-bundle`.
fn build_bundle(root: &Path, release: bool) -> Result<bundle::Sources, String> {
    let art = crate::image::build(root, crate::image::BuildOpts { release, ktest: false })?;
    let d = xdir(root).join("stick-bundle");
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    let cp = |from: &Path, to: &str| std::fs::copy(from, d.join(to)).map(|_| ()).map_err(|e| format!("{}: {e}", from.display()));
    cp(&art.kernel, "KERNEL")?;
    cp(&art.bitsboot, "BOOTX64.EFI")?;
    std::fs::write(d.join("INITRD"), crate::image::initrd_bytes_for(root, &art.programs, &art.kernel)?).map_err(|e| e.to_string())?;
    println!("bundle:   {}", d.display());
    Ok(bundle::Sources::from_dir(&d))
}

fn yes(answer: &str) -> bool {
    matches!(answer.trim(), "J" | "j")
}

/// Checks the stick again after the question and refuses if anything the plan relied on changed.
fn recheck(p: &dyn Probe, o: &Opts, id: &Ident) -> Result<Ident, String> {
    let again = gate::identify(p, &o.target, o.recorded, o.force).map_err(|e| format!("the stick changed while the question waited: {e}"))?;
    if !again.same_stick(id) || again.loader_bytes != id.loader_bytes || again.v1_dir != id.v1_dir {
        return Err("refused: the stick is no longer the one shown above (swapped or changed). Nothing was written.".into());
    }
    Ok(again)
}

/// One run per stick at a time: an exclusive lock on a file outside the stick, named by the stick's UUID,
/// held from the first look at the stick to the end of the run (dropped with the returned file).
fn lock(o: &Opts, id: &Ident) -> Result<std::fs::File, String> {
    let key: String = id.blk.uuid.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    let key = if key.is_empty() { id.dev.rsplit('/').next().unwrap_or("stick").to_string() } else { key };
    let path = o.lock_dir.join(format!("bitsos-stick-{key}.lock"));
    let f = std::fs::File::create(&path).map_err(|e| format!("lock {}: {e}", path.display()))?;
    // A bounded wait: a process forked elsewhere in this program shares the lock until its exec.
    for _ in 0..40 {
        match f.try_lock() {
            Ok(()) => return Ok(f),
            Err(std::fs::TryLockError::WouldBlock) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(std::fs::TryLockError::Error(e)) => return Err(format!("lock {}: {e}", path.display())),
        }
    }
    Err(format!("refused: another `xtask stick` run is working on this stick (lock {}). Nothing was written.", path.display()))
}

pub fn run(p: &dyn Probe, o: &Opts, ask: &mut dyn FnMut(&str) -> String) -> Result<(), String> {
    let first = gate::identify(p, &o.target, o.recorded, o.force)?;
    let _lock = lock(o, &first)?;
    // Looked at again under the lock: a run that just finished may have changed the loader or the markers.
    let id = gate::identify(p, &o.target, o.recorded, o.force)?;
    if !id.same_stick(&first) {
        return Err("refused: the stick changed while the tool waited for its lock. Nothing was written.".into());
    }
    println!("stick: {}", id.describe());
    if let Some(b) = &id.forced_bus {
        println!("stick: WARNING: the stick is on {b}, not USB; accepted because of --force");
    }
    let place = if id.id_from_v1 { "\\BITSOS\\STICK.ID (V1's place)" } else { "\\BITSOS2\\STICK.ID" };
    println!("stick: identity {} in {place} equals the recorded ID ({})", gate::id_hex(&id.id), o.record_src);
    let loader = match id.loader {
        Loader::None => "none",
        Loader::V1 => "V1's loader",
        Loader::V2 => "V2 bitsboot",
        Loader::Foreign => "foreign",
    };
    println!("stick: \\EFI\\BOOT\\BOOTX64.EFI: {loader}");
    if o.retire {
        return retire_flow(p, o, id, ask);
    }
    if id.v1_dir {
        println!("stick: V1 is still on this stick (\\BITSOS\\)");
        if o.has_sys_ops() {
            return Err("refused: the stick takes V2 only: retire V1 first (--retire-v1). Nothing was written.".into());
        }
        println!("next: --retire-v1 (copies the stick, compares, then removes V1)");
        return Ok(());
    }
    let st = sys::scan(&id.point)?;
    let (keep, keep_why) = match o.sys_keep {
        Some(k) => (k.max(1), "--sys-keep".to_string()),
        None => sys::sys_keep(&id.point)?,
    };
    let pin = match &st.pin {
        sys::Pin::None => "none".to_string(),
        sys::Pin::Number(n) => n.to_string(),
        sys::Pin::Invalid => "invalid (ignored by bitsboot)".to_string(),
    };
    println!("stick: generations: {}; PIN {pin}; sys_keep {keep} ({keep_why})", st.summary());
    if !o.has_sys_ops() {
        match st.newest() {
            Some(g) if !g.ok => println!("stick: a bundle would take the place of generation {} (never confirmed)", g.number),
            Some(g) => println!("stick: a bundle would become generation {}", g.number as u64 + 1),
            None => println!("stick: a bundle would become generation 1"),
        }
        println!("nothing to do: give a bundle (--bundle/--build), --confirm <n>, --prune or --unpin");
        return Ok(());
    }

    let bundle = o.bundle.as_ref().map(bundle::load).transpose()?;
    let config = match &o.config {
        Some(f) => {
            let t = std::fs::read(f).map_err(|e| format!("--config {}: {e}", f.display()))?;
            bundle::check_config(&t).map_err(|e| format!("refused: --config {}: {e}", f.display()))?;
            Some((t, f.clone()))
        }
        None => None,
    };
    let mut pl = sys::Planner::new(&id.point, st.clone())?;
    if o.unpin {
        pl.unpin();
    }
    if let Some(n) = o.confirm {
        pl.confirm(n)?;
    }
    if let Some(b) = &bundle {
        let n = pl.bundle(b, o.proven, id.loader_bytes.as_deref(), o.force)?;
        println!("stick: the bundle becomes generation {n} ({})", if o.proven { "OK: Reload-proven" } else { "NEW: one try at the next start" });
    }
    if let Some((t, f)) = &config {
        pl.config(t, f);
    }
    pl.finish(keep);
    for n in &pl.notes {
        println!("note: {n}");
    }
    if pl.steps.is_empty() {
        println!("nothing to write");
        return Ok(());
    }
    println!("plan:");
    for (i, s) in pl.steps.iter().enumerate() {
        println!("  {:>2}. {s}", i + 1);
    }
    println!("afterwards: {}", pl.st.summary());
    println!("nothing else on the stick changes");
    if !o.write {
        println!("DRY RUN: nothing was written. Add --write to write; it asks for J first.");
        return Ok(());
    }
    if id.read_only {
        return Err(format!("refused: {} is mounted read-only. Nothing was written.", id.point.display()));
    }
    let need: u64 = pl.steps.iter().map(|s| if let sys::Step::Write { bytes, .. } = s { bytes.len() as u64 } else { 0 }).sum();
    let free = p.free_bytes(&id.point)?;
    if need.saturating_add(1 << 20) > free {
        return Err(format!("refused: the stick has {free} bytes free, the plan writes {need} (lower sys_keep and --prune first). Nothing was written."));
    }
    if !yes(&ask("Type J and Enter to write, anything else cancels: ")) {
        return Err("cancelled, nothing was written".into());
    }
    let again = recheck(p, o, &id)?;
    if sys::scan(&again.point)? != st {
        return Err("refused: the generations on the stick changed while the question waited. Nothing was written.".into());
    }
    sys::apply(&again.point, &pl.steps)?;
    let after = sys::scan(&again.point)?;
    let looks = |s: &sys::SysState| s.gens.iter().map(|g| (g.number, g.kernel, g.initrd, g.new, g.tried, g.ok)).collect::<Vec<_>>();
    if looks(&after) != looks(&pl.st) || after.staging.is_some() {
        return Err(format!("RED: the stick does not look as planned: {} (planned: {})", after.summary(), pl.st.summary()));
    }
    println!("stick: generations now: {}", after.summary());
    println!("done. Unmount before pulling the stick: udisksctl unmount -b {}", again.dev);
    Ok(())
}

fn retire_flow(p: &dyn Probe, o: &Opts, id: Ident, ask: &mut dyn FnMut(&str) -> String) -> Result<(), String> {
    let plan = retire::plan(&id, &o.backup, |a| p.free_bytes(a))?;
    retire::show(&plan, &id);
    if !o.write {
        println!("DRY RUN: nothing was copied or removed. Add --write; it asks for J first.");
        return Ok(());
    }
    if id.read_only {
        return Err(format!("refused: {} is mounted read-only. Nothing was written.", id.point.display()));
    }
    if !yes(&ask("Type J and Enter to copy the stick and then remove V1, anything else cancels: ")) {
        return Err("cancelled, nothing was copied or removed".into());
    }
    let again = recheck(p, o, &id)?;
    retire::backup(&again, &plan)?;
    // The copy takes minutes: the stick is checked once more right before anything on it changes.
    let again = recheck(p, o, &again)?;
    if retire::walk(&again.point)? != plan.tree {
        return Err("RED: the stick changed after the copy; V1 stays".into());
    }
    retire::remove_v1(&again, &plan)?;
    let after = retire::walk(&again.point)?;
    if after != expected_after_retire(&plan, &id) {
        return Err(format!("RED: the stick does not look as planned after removing V1; the backup is in {}", plan.backup.display()));
    }
    println!("retire-v1 done: backup {} (compare green), only V1's paths are gone", plan.backup.display());
    println!("next: the first bundle with bitsboot, e.g. cargo xtask stick --target {} --build --release --write", again.point.display());
    Ok(())
}

/// The stick after --retire-v1: as before, minus `\BITSOS\` and V1's loader, plus `\BITSOS2\STICK.ID`.
fn expected_after_retire(plan: &retire::Plan, id: &Ident) -> retire::Tree {
    let gone = |r: &String| plan.v1_paths.contains(r) || plan.loader.as_ref() == Some(r);
    let mut t = retire::Tree {
        dirs: plan.tree.dirs.iter().filter(|r| !gone(r)).cloned().collect(),
        files: plan.tree.files.iter().filter(|(r, _)| !gone(r)).cloned().collect(),
    };
    let b2 = t.dirs.iter().find(|d| d.eq_ignore_ascii_case("BITSOS2")).cloned();
    let b2 = b2.unwrap_or_else(|| {
        t.dirs.push("BITSOS2".into());
        "BITSOS2".into()
    });
    if !t.files.iter().any(|(f, _)| f.eq_ignore_ascii_case(&format!("{b2}/STICK.ID"))) {
        t.files.push((format!("{b2}/STICK.ID"), id.id_bytes.len() as u64));
    }
    t.dirs.sort();
    t.files.sort();
    t
}
