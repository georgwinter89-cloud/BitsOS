//! `--qemu`: BitsOS in a window. QEMU is found (or installed after a yes), the release goes into a sparse image
//! file with the stick's own writer (MBR, FAT32 labelled BITSOS, STICK.ID, CONFIG.TXT), and a start script and
//! shortcuts run QEMU on it. No disk and no stick is touched. A second run adds the release as a new generation.

pub mod args;
pub mod files;
pub mod host;
mod image;
mod place;
pub mod setup;
pub mod win;
#[cfg(test)]
mod tests;

use crate::cli::Opts;
use crate::config::{self, Answers};
use crate::manifest::{Kind, Manifest};
use crate::{devices, flow, release, secrets, stick, ui};
use args::{Accel, Os, Vm};
use files::Layout;
use image::{full_disk, plain, private, room};
use setup::{Found, OS};
use std::path::{Path, PathBuf};

pub const DEFAULT_SIZE: u64 = 16 << 30;
pub const DEFAULT_SCREEN: &str = "1600x900";
/// The first host port tried for the remote control's forward (outside xtask's 20000..20499).
const FIRST_PORT: u16 = 18765;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok()
}

fn terminal() -> bool {
    std::io::IsTerminal::is_terminal(&std::io::stdin())
}

pub fn run(o: &Opts) -> Result<(), String> {
    let q = o.qemu.clone().unwrap_or_default();
    println!("BitsOS installer {} ({}): BitsOS in a window (QEMU)", release::VERSION, release::REPOSITORY);
    if OS == Os::Linux && std::fs::read_to_string("/proc/self/status").ok().and_then(|s| host::euid(&s)) == Some(0) {
        return Err("run --qemu without sudo: it writes only into your own folders and asks for root itself when QEMU has to be installed".into());
    }
    if !o.batch() && !terminal() {
        return Err(if q.uninstall { "no terminal to ask at: --uninstall --yes removes without asking".into() } else { "no terminal to ask at: give --remote yes|no and --start yes|no (and --install-qemu if QEMU may be installed); --qemu --help".to_string() });
    }
    let dir = q.image_dir.clone().or_else(|| files::default_dir(OS, &env)).ok_or("no data folder found (HOME or LOCALAPPDATA): give --image-dir")?;
    let l = Layout { dir, os: OS };
    if setup::running(&l.pidfile(), &l.image()) {
        return Err(format!("BitsOS is running in QEMU from {}: close its window (or type poweroff in BitsOS) first. Nothing was changed.", l.image().display()));
    }
    if q.uninstall {
        return place::uninstall(&l, q.yes, q.dry_run);
    }
    let (f, cache) = flow::fetcher(o).or_else(|e| if q.dry_run { Ok((crate::fetch::Fetcher { assets: None, cache: PathBuf::new(), vendor_dirs: vec![], offline: true }, PathBuf::new())) } else { Err(e) })?;
    let update = l.image().is_file();
    let old_script = std::fs::read_to_string(l.script()).unwrap_or_default();
    // An image already there is updated, and only when it is BitsOS's; that is known before any download.
    let mut existing = None;
    if update {
        let mut img = devices::open_image(&l.image(), None).map_err(plain)?;
        match stick::inspect(&mut img)? {
            Some(e) if e.is_bitsos() => println!("image: {} holds BitsOS (generations: {}); this release is added, CONFIG.TXT and data.img stay", l.image().display(), e.sys.summary()),
            _ => return Err(format!("refused: {} is not a BitsOS image; `bitsos-install --qemu --uninstall` removes it. Nothing was written.", l.image().display())),
        }
        existing = Some(img);
    }
    // The release is checked before QEMU is installed: a run that stops later has installed nothing.
    let manifest = if q.dry_run { None } else { Some(flow::manifest(o, &f)?) };
    let found = qemu_ready(o, &q, &cache)?;
    let (accel, slow) = accel();

    // The answers, all before anything is written.
    let remote = match (update, o.remote) {
        (true, _) => l.token().is_file() || args::port_in(&old_script).is_some(),
        (false, Some(r)) => r,
        (false, None) if q.dry_run => false,
        (false, None) => {
            println!("\nRemote control lets an AI helper on this computer talk to BitsOS in the window (TLS, paired once in the");
            println!("BitsOS window; QEMU forwards a port on 127.0.0.1 only).");
            ui::yes("Turn remote control on?", false)?
        }
    };
    if update && o.remote.is_some_and(|r| r != remote) {
        println!("note: remote control stays {} as the image's CONFIG.TXT has it (mcp= is changed by hand only)", if remote { "on" } else { "off" });
    }
    let port = if remote { Some(args::port_in(&old_script).unwrap_or_else(free_port)) } else { None };
    let start = match q.start {
        Some(s) => s,
        None if q.dry_run => false,
        None => ui::yes("Start BitsOS when it is ready?", true)?,
    };
    let host_mem = if OS == Os::Windows { windows_memory() } else { std::fs::read_to_string("/proc/meminfo").ok().and_then(|t| host::meminfo_bytes(&t)) };
    let vm = Vm {
        os: OS,
        qemu: found.qemu.clone(),
        code: found.code.clone(),
        vars: l.vars(),
        image: l.image(),
        pidfile: l.pidfile(),
        mem_mb: host::mem_mb(host_mem),
        cpus: host::cpus(std::thread::available_parallelism().map_or(2, |n| n.get())),
        accel,
        display: found.display.clone(),
        mcp_port: port,
    };
    if q.dry_run {
        println!("\n{} {}", if update { "would add this release as a new generation to" } else { "would write a new image" }, l.image().display());
        println!("QEMU: {}\nfirmware: {} (variables from {})\nscreen: {}", found.qemu.display(), found.code.display(), found.vars.display(), o.screen.as_deref().unwrap_or(DEFAULT_SCREEN));
        println!("start script: {}\ncommand:\n  {}", l.script().display(), args::full(&vm, &l.log()).join(" "));
        if let Some(s) = slow {
            println!("note: {s}");
        }
        println!("dry run: nothing was written or installed.");
        return Ok(());
    }

    let Some(m) = manifest else { return Err("internal: no release manifest".into()) };
    println!("release {}: {} assets, manifest checked", m.version, m.assets.len());
    let mut notes = Vec::new();
    let (spec, steam, brave) = programs(&m, o)?;
    let mut o2 = o.clone();
    o2.trees = Some(spec);
    let trees = flow::choose_trees(&m, &o2, steam, brave, &mut notes)?;
    let size = q.size.unwrap_or(DEFAULT_SIZE);
    if !update {
        let bytes = m.system().unpacked + trees.iter().map(|a| a.unpacked + a.vendor_bytes).sum::<u64>();
        let files = m.system().files + trees.iter().map(|a| a.files + a.vendor_files).sum::<u64>();
        stick::check_fresh_space(size, bytes, files).map_err(|e| format!("{e}\nGive a bigger --size or fewer programs."))?;
        room(&l.dir, stick::fresh_need(size, bytes, files)?, size)?;
    }
    let (plan, missing) = flow::gather(&o2, &m, &f, &cache, &trees, steam, brave)?;

    #[cfg(unix)]
    let new_dir = !l.dir.exists();
    std::fs::create_dir_all(&l.dir).map_err(|e| format!("{}: {e}", l.dir.display()))?;
    #[cfg(unix)]
    if new_dir {
        // The token lies here; a folder the person named that existed before keeps its access.
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&l.dir, std::fs::Permissions::from_mode(0o700));
    }
    let mut log = |s: &str| println!("{s}");
    let done = if let Some(mut img) = existing {
        private(&l.image());
        stick::update(&mut img, &plan, &mut log).map_err(|e| full_disk(e, &l.dir))?
    } else {
        let token = if remote { Some(secrets::new_token()?) } else { None };
        if let Some(t) = &token {
            secrets::write_private(&l.token(), t)?;
            l.remember(&l.token())?;
        }
        let screen = Some(o.screen.clone().filter(|s| s != "auto").unwrap_or_else(|| DEFAULT_SCREEN.into()));
        let cfg = config::render(&Answers { token, screen, qemu: true, ..Answers::default() }, release::VERSION)?;
        let part = l.image_part();
        let _ = std::fs::remove_file(&part);
        let mut img = devices::open_image(&part, Some(size)).map_err(|e| full_disk(plain(e), &l.dir))?;
        private(&part);
        let d = stick::fresh(&mut img, &plan, &secrets::new_stick_id()?, &cfg, &mut log).map_err(|e| full_disk(e, &l.dir))?;
        drop(img);
        std::fs::rename(&part, l.image()).map_err(|e| format!("{}: {e}", l.image().display()))?;
        d
    };
    l.remember(&l.image())?;
    if !l.vars().is_file() {
        std::fs::copy(&found.vars, l.vars()).map_err(|e| format!("copy {} to {}: {e}", found.vars.display(), l.vars().display()))?;
    }
    l.remember(&l.vars())?;
    let shortcuts = place::launchers(&l, &vm)?;
    notes.extend(done.notes.iter().cloned());

    println!("\nDone: {} files ({} MB) written and read back into {}; generations: {}.", done.files, done.bytes >> 20, l.image().display(), done.sys);
    for n in &notes {
        println!("note: {n}");
    }
    if !missing.is_empty() {
        println!("Not in the image: {}", missing.iter().map(|m| m.split(':').next().unwrap_or("")).collect::<Vec<_>>().join(", "));
    }
    println!("Start BitsOS with {}", l.script().display());
    for s in &shortcuts {
        println!("  or the shortcut {}", s.display());
    }
    if let Some(s) = slow {
        println!("note: QEMU emulates the processor (slow: minutes to the desktop): {s}");
    }
    if let Some(p) = port {
        println!("\nRemote control: QEMU forwards 127.0.0.1:{p} to BitsOS's port {}; the token is in {}.", args::GUEST_MCP_PORT, l.token().display());
        println!("Pair once after BitsOS started: the BitsOS window shows `bitsmcp: TLS server key sha256 <hex>` (also in {}).", l.log().display());
        println!("  With host/mcp/ from {} (Node.js):", release::REPOSITORY);
        println!("  node host/mcp/pair.mjs 127.0.0.1:{p} --expect <hex> --token-file {}", l.token().display());
        println!("  and answer the question in the BitsOS window.");
    }
    println!("\nThe keyboard layout is German (QWERTZ): on a US keyboard Y and Z are swapped. The mouse moves freely in and out");
    println!("of the window. To end BitsOS, type poweroff at a console or close the window.");
    if start {
        let pid = setup::start(&l.script(), &l.output())?;
        l.remember(&l.output())?;
        println!("BitsOS starts (process {pid}; QEMU's messages go to {}).", l.output().display());
    }
    Ok(())
}

/// QEMU, its firmware and its window, installed first when missing and the person (or --install-qemu) agrees.
fn qemu_ready(o: &Opts, q: &crate::cli::QemuOpts, cache: &Path) -> Result<Found, String> {
    let lack = match setup::detect() {
        Ok(f) => return Ok(f),
        Err(l) => l,
    };
    println!("\nMissing on this computer: {}.", lack.join(", "));
    let (what, line) = setup::install_text()?;
    println!("{what}\n  {line}");
    if q.dry_run {
        let would = match OS {
            Os::Windows => win::install_in(&setup::windows_default_dir()),
            Os::Linux => win::Install { exe: "qemu-system-x86_64".into(), code: "<OVMF code>".into(), vars: "<OVMF variables>".into() },
        };
        println!("dry run: nothing is installed; the command below uses the paths it would have.");
        return Ok(Found { qemu: would.exe, code: would.code, vars: would.vars, display: setup::window("gtk") });
    }
    let yes = if q.install_qemu {
        true
    } else if o.batch() {
        return Err(format!("stopped: QEMU is missing. Install it yourself ({line}), or run again with --install-qemu. Nothing was written."));
    } else {
        ui::yes("Install it now?", false)?
    };
    if !yes {
        return Err("stopped: QEMU is needed. Nothing was written.".into());
    }
    setup::install(cache, o.offline, terminal())?;
    setup::detect().map_err(|l| format!("still missing after the installation: {}. Nothing was written.", l.join(", ")))
}

/// The accelerator for the command shown and the note on a slow start. The launchers decide again at each start
/// (Linux: /dev/kvm; Windows: WHPX with TCG behind it), so fixing the cause needs no new run of the installer.
fn accel() -> (Accel, Option<String>) {
    match OS {
        Os::Linux => match host::kvm(Path::new("/")) {
            host::Kvm::Usable => (Accel::Kvm, None),
            k => (Accel::Tcg, k.note()),
        },
        Os::Windows if windows_whpx() => (Accel::Whpx, None),
        Os::Windows => (
            Accel::Whpx,
            Some("the Windows Hypervisor Platform is off: turn on \"Windows-Hypervisor-Plattform\" (Windows Hypervisor Platform) in optionalfeatures.exe and restart Windows; BitsOS uses it from its next start".into()),
        ),
    }
}

#[cfg(windows)]
fn windows_whpx() -> bool {
    devices::windows::vm::whpx_present()
}
#[cfg(not(windows))]
fn windows_whpx() -> bool {
    false
}
#[cfg(windows)]
fn windows_memory() -> Option<u64> {
    devices::windows::vm::memory_bytes()
}
#[cfg(not(windows))]
fn windows_memory() -> Option<u64> {
    None
}

fn free_port() -> u16 {
    (FIRST_PORT..FIRST_PORT + 100).find(|p| std::net::TcpListener::bind(("127.0.0.1", *p)).is_ok()).unwrap_or(FIRST_PORT)
}

/// The program trees: none unless asked for; every one is untested in QEMU.
fn programs(m: &Manifest, o: &Opts) -> Result<(String, bool, bool), String> {
    let (mut steam, mut brave) = (o.steam.unwrap_or(false), o.brave.unwrap_or(false));
    if let Some(t) = &o.trees {
        return Ok((t.clone(), steam, brave));
    }
    let mut want = Vec::new();
    let offered: Vec<_> = m.trees().filter(|a| a.tree.is_some()).collect();
    if o.batch() || offered.is_empty() {
        return Ok(("none".into(), steam, brave));
    }
    println!("\nThe image gets the system and the desktop. The program trees are made for the real machine and are untested in QEMU.");
    if !ui::yes("Add any of them anyway?", false)? {
        return Ok(("none".into(), steam, brave));
    }
    for a in offered {
        let t = a.tree.clone().unwrap_or_default();
        let maker = if a.kind == Kind::Runtime { format!(", {} MB more from its maker", a.vendor_bytes >> 20) } else { String::new() };
        let preset = match t.as_str() {
            "steam" => o.steam,
            "brave" => o.brave,
            _ => None,
        };
        let yes = match preset {
            Some(v) => v,
            None => ui::yes(&format!("  {t} ({} MB{maker}, untested in QEMU)?", a.unpacked >> 20), false)?,
        };
        match t.as_str() {
            "steam" => steam = yes,
            "brave" => brave = yes,
            _ if yes => want.push(t),
            _ => {}
        }
    }
    Ok((if want.is_empty() { "none".into() } else { want.join(",") }, steam, brave))
}
