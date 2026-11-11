//! One run of the installer: the release manifest, the stick, the human's confirmation, the questions, the space
//! check, the downloads and checks, the host's record of the stick, and only then the write. What the installer can
//! know before the write it checks before the write (the archives themselves are checked when a release is made,
//! `xtask release-assets`); what fails after it says what state the stick is in.

use crate::cli::{Net, Opts};
use crate::config::{self, Answers, Wifi};
use crate::confirm::{self, Action};
use crate::devices::{self, Disk};
use crate::fetch::{self, Fetcher};
use crate::manifest::{self, Kind, Manifest};
use crate::stick::{self, Plan, TreeInput};
use crate::{release, secrets, ui, vendor};

pub(crate) fn manifest(o: &Opts, f: &Fetcher) -> Result<Manifest, String> {
    let expected = match (release::EMBEDDED_MANIFEST_SHA256, &o.manifest_sha256) {
        (Some(_), Some(_)) => return Err("this installer carries its release's manifest hash; --manifest-sha256 cannot replace it".into()),
        (Some(h), None) => h.to_string(),
        (None, Some(h)) if o.assets.is_some() && crate::sha::is_hex64(h) => {
            println!("note: this installer was built without a release; the manifest is checked against the hash you gave.");
            h.clone()
        }
        (None, _) => return Err("this installer was built without a release: use --assets DIR --manifest-sha256 <hex> (cargo xtask release-assets prints both)".into()),
    };
    let m = manifest::verify(&f.manifest(&release::asset_url(manifest::FILE))?, &expected)?;
    if release::EMBEDDED_MANIFEST_SHA256.is_some() && m.version != release::VERSION {
        return Err(format!("refused: the manifest is release {}, this installer installs {}", m.version, release::VERSION));
    }
    Ok(m)
}

fn disks(o: &Opts) -> Result<(Vec<Disk>, Box<dyn devices::Devices>), String> {
    if o.image.is_some() {
        return Ok((Vec::new(), Box::new(NoDisks)));
    }
    let d = devices::system()?;
    Ok((d.list()?, d))
}

struct NoDisks;
impl devices::Devices for NoDisks {
    fn list(&self) -> Result<Vec<Disk>, String> {
        Ok(Vec::new())
    }
    fn open(&self, _: &Disk) -> Result<Box<dyn devices::BlockDev>, String> {
        Err("no disks".into())
    }
}

fn show(list: &[Disk]) {
    let offered: Vec<&Disk> = list.iter().filter(|d| d.offered()).collect();
    if offered.is_empty() {
        println!("No USB stick found that could be written. Plug one in (it must be a removable USB disk).");
    }
    for (i, d) in offered.iter().enumerate() {
        println!("  [{}] {}", i + 1, d.describe());
    }
    for d in list.iter() {
        if let Some(why) = d.why_not() {
            println!("  not offered: {} ({why})", d.describe());
        }
    }
}

/// The trees to install, with what they need added.
pub(crate) fn choose_trees(m: &Manifest, o: &Opts, steam: bool, brave: bool, notes: &mut Vec<String>) -> Result<Vec<manifest::Asset>, String> {
    let want: Option<Vec<String>> = match o.trees.as_deref() {
        None | Some("all") => None,
        Some("none") => Some(Vec::new()),
        Some(l) => Some(l.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()),
    };
    if let Some(w) = &want
        && let Some(bad) = w.iter().find(|t| !m.trees().any(|a| a.tree.as_deref() == Some(t.as_str())))
    {
        return Err(format!("--trees: the release has no tree '{bad}'"));
    }
    let picked = |a: &manifest::Asset| {
        let t = a.tree.as_deref().unwrap_or("");
        match a.kind {
            Kind::Runtime => (t == "steam" && steam) || (t == "brave" && brave) || (t != "steam" && t != "brave"),
            _ => want.as_ref().is_none_or(|w| w.iter().any(|x| x == t)),
        }
    };
    let mut out: Vec<manifest::Asset> = m.trees().filter(|a| picked(a)).cloned().collect();
    loop {
        let missing: Vec<String> = out.iter().flat_map(|a| a.needs.clone()).filter(|n| !out.iter().any(|b| b.tree.as_deref() == Some(n))).collect();
        let Some(n) = missing.first() else { break };
        let a = m.trees().find(|a| a.tree.as_deref() == Some(n.as_str())).ok_or_else(|| format!("the release lacks the tree {n} another tree needs"))?;
        notes.push(format!("{n} is added: another chosen tree runs on it"));
        out.push(a.clone());
    }
    out.sort_by(|a, b| a.tree.cmp(&b.tree));
    Ok(out)
}

/// The release files of the system and `trees`, fetched and checked, with Steam's and Brave's files from their makers;
/// what was asked for and cannot be installed is listed (and stops the run unless --allow-missing or a yes).
pub(crate) fn gather(o: &Opts, m: &Manifest, f: &Fetcher, cache: &std::path::Path, trees: &[manifest::Asset], steam: bool, brave: bool) -> Result<(Plan, Vec<String>), String> {
    let system = f.asset(m.system(), &release::asset_url(&m.system().name))?;
    let mut plan = Plan { system, system_asset: m.system().name.clone(), trees: Vec::new(), bytes: m.system().unpacked, files: m.system().files };
    let mut missing = Vec::new();
    for (wanted, t) in [(steam, "steam"), (brave, "brave")] {
        if wanted && !trees.iter().any(|a| a.tree.as_deref() == Some(t)) {
            missing.push(format!("{t}: this release does not carry it"));
        }
    }
    for a in trees {
        let name = a.tree.clone().expect("a tree asset");
        let path = f.asset(a, &release::asset_url(&a.name))?;
        let mut vendor_files = Vec::new();
        if a.kind == Kind::Runtime {
            let text = crate::archive::member(&path, "VENDOR.txt", 1 << 20)?.ok_or_else(|| format!("{}: no VENDOR.txt", a.name))?;
            let l = vendor::List::parse(&String::from_utf8_lossy(&text))?;
            println!("{name}: {} files ({} MB) come from their maker", l.files.len(), l.bytes() >> 20);
            match vendor::resolve(&l, f, &cache.join("staged").join(&name)) {
                Ok(v) => vendor_files = v,
                Err(e) => {
                    missing.push(format!("{name}: {e}"));
                    continue;
                }
            }
        }
        plan.bytes += a.unpacked + vendor_files.iter().map(|v| v.size).sum::<u64>();
        plan.files += a.files + vendor_files.len() as u64;
        plan.trees.push(TreeInput { name, archive: path, asset: a.name.clone(), vendor: vendor_files });
    }
    // A tree whose base was left out cannot run either.
    let have: Vec<String> = plan.trees.iter().map(|t| t.name.clone()).collect();
    for t in trees {
        let n = t.tree.clone().unwrap_or_default();
        if have.contains(&n) && t.needs.iter().any(|x| !have.contains(x)) {
            missing.push(format!("{n}: left out because a tree it runs on is missing"));
        }
    }
    plan.trees.retain(|t| !missing.iter().any(|x| x.starts_with(&format!("{}: left out", t.name))));
    for x in &missing {
        println!("NOT INSTALLED: {x}");
    }
    // A program the person asked for is not dropped behind their back, least of all for an agent reading exit codes.
    if !missing.is_empty() && !o.allow_missing {
        if o.batch() {
            return Err("stopped: a tree that was asked for cannot be installed (NOT INSTALLED above). Nothing was written. --allow-missing goes on without it.".into());
        }
        if !ui::yes("Go on without it?", false)? {
            return Err("stopped; nothing was written".into());
        }
    }

    Ok((plan, missing))
}

fn answers(o: &Opts) -> Result<Answers, String> {
    let mut a = Answers { extra: o.extra_config.clone(), ..Answers::default() };
    a.wifi = match &o.net {
        Net::Cable => None,
        Net::Wifi { ssid, psk_file } => {
            let t = std::fs::read_to_string(psk_file).map_err(|e| format!("--wifi-psk-file {}: {e}", psk_file.display()))?;
            Some(Wifi { ssid: ssid.clone(), psk: t.lines().next().unwrap_or("").to_string() })
        }
        Net::Ask => {
            println!("\nNetwork. A cable (Intel I226 or 82576 Ethernet) works by itself. Wi-Fi works with the RTL8852BE only.");
            if ui::yes("Set up Wi-Fi?", false)? {
                loop {
                    let ssid = ui::line("Wi-Fi network name: ")?;
                    let psk = ui::secret("Wi-Fi passphrase (not shown): ")?;
                    match config::check_ssid(&ssid).and_then(|_| config::check_psk(&psk)) {
                        Ok(()) => break Some(Wifi { ssid, psk }),
                        Err(e) => println!("{e}; once more."),
                    }
                }
            } else {
                None
            }
        }
    };
    let remote = match o.remote {
        Some(r) => r,
        None => {
            println!("\nRemote control lets an AI helper on your computer talk to BitsOS over the network (TLS; you pair it");
            println!("once at the BitsOS machine's keyboard, and every install or full access is asked there again).");
            ui::yes("Turn remote control on?", false)?
        }
    };
    if remote {
        a.token = Some(secrets::new_token()?);
    }
    a.screen = match o.screen.as_deref() {
        Some("auto") => None,
        Some(s) => Some(s.to_string()),
        None if o.batch() => None,
        None => {
            let s = ui::line("\nScreen mode [Enter = 2560x1440 if the screen offers it, else the firmware's mode; largest; or WxH like 1920x1080]: ")?;
            if s.trim().is_empty() { None } else { config::check_screen(s.trim()).map(|_| Some(s.trim().to_string()))? }
        }
    };
    Ok(a)
}

/// Where the release comes from and where downloads are kept (`<cache>/<version>`).
pub(crate) fn fetcher(o: &Opts) -> Result<(Fetcher, std::path::PathBuf), String> {
    if o.assets.is_none() && !o.offline {
        fetch::curl_ready()?;
    }
    let cache = o.cache.clone().unwrap_or_else(fetch::default_cache).join(release::VERSION);
    Ok((Fetcher { assets: o.assets.clone(), cache: cache.clone(), vendor_dirs: o.vendor_cache.clone(), offline: o.offline }, cache))
}

pub fn run(o: Opts) -> Result<(), String> {
    println!("BitsOS installer {} ({})", release::VERSION, release::REPOSITORY);
    if !o.batch() && !o.list && !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return Err("no terminal to ask at: use --list, then --erase <id> (or --update <id>) with every answer as a flag (--help)".into());
    }
    let (list, devs) = disks(&o)?;
    if o.list {
        show(&list);
        return Ok(());
    }
    let (f, cache) = fetcher(&o)?;
    let m = manifest(&o, &f)?;
    println!("release {}: {} assets, manifest checked", m.version, m.assets.len());

    // The stick.
    let (disk, mut dev): (Disk, Box<dyn devices::BlockDev>) = match &o.image {
        Some(p) => {
            let img = devices::open_image(p, if o.update.is_some() { None } else { o.image_size })?;
            let d = devices::image_disk(p, devices::BlockDev::size(&img));
            (d, Box::new(img))
        }
        None => {
            let d = match o.erase.as_ref().or(o.update.as_ref()) {
                Some(id) => confirm::pick(&list, id)?.clone(),
                None => {
                    show(&list);
                    let offered: Vec<&Disk> = list.iter().filter(|d| d.offered()).collect();
                    if offered.is_empty() {
                        return Err("nothing was written".into());
                    }
                    let n = ui::line("Number of the stick to use: ")?;
                    let i: usize = n.trim().parse().ok().filter(|i| (1..=offered.len()).contains(i)).ok_or("no such number; nothing was written")?;
                    offered[i - 1].clone()
                }
            };
            let dev = devs.open(&d)?;
            (d, dev)
        }
    };
    if let Some(id) = o.erase.as_ref().or(o.update.as_ref())
        && *id != disk.id
    {
        return Err(format!("refused: '{id}' is not this stick's id {}. Nothing was written.", disk.id));
    }
    println!("\nstick: {}", disk.describe());
    let existing = stick::inspect(dev.as_mut())?;
    let bitsos = existing.as_ref().filter(|e| e.is_bitsos());
    if let Some(e) = bitsos {
        println!("stick: it already holds BitsOS (generations: {})", e.sys.summary());
    }
    let action = match (&o.erase, &o.update) {
        (Some(_), _) => Action::Erase,
        (_, Some(_)) => Action::Update,
        _ if bitsos.is_some() && ui::yes("Keep its settings and data, and add this release as a new generation?", true)? => Action::Update,
        _ => Action::Erase,
    };
    if action == Action::Update && bitsos.is_none() {
        return Err("refused: --update needs a BitsOS stick (label BITSOS and \\BITSOS2\\STICK.ID); this one is not. Nothing was written.".into());
    }
    if !o.batch() {
        let what = if action == Action::Erase { "EVERYTHING ON THIS STICK WILL BE ERASED." } else { "The new release is added; CONFIG.TXT, data.img and the older generations stay." };
        println!("{what}\nType {} {} and Enter to go on; anything else stops.", action.word(), disk.id);
        if !confirm::typed_ok(action, &disk, &ui::line("> ")?) {
            return Err("stopped; nothing was written".into());
        }
    } else if !o.missing().is_empty() {
        return Err(format!("missing answers: {}", o.missing().join(", ")));
    }

    // Questions, downloads and checks: all before the stick is touched.
    let ans = if action == Action::Erase { Some(answers(&o)?) } else { None };
    let steam = match o.steam {
        Some(s) => s,
        None => ui::yes("\nAdd Steam? Valve's client (about 720 MB) is downloaded from Valve on this computer", false)?,
    };
    let brave = match o.brave {
        Some(b) => b,
        None => ui::yes("Add the Brave browser? It is downloaded from Brave's GitHub releases on this computer", true)?,
    };
    let mut notes = Vec::new();
    let trees = choose_trees(&m, &o, steam, brave, &mut notes)?;
    if action == Action::Erase {
        let bytes = m.system().unpacked + trees.iter().map(|a| a.unpacked + a.vendor_bytes).sum::<u64>();
        let files = m.system().files + trees.iter().map(|a| a.files + a.vendor_files).sum::<u64>();
        stick::check_fresh_space(disk.size, bytes, files).map_err(|e| format!("{e}\nChoose fewer trees (--trees, or no to Steam or Brave), or use a bigger stick."))?;
    }
    let (plan, missing) = gather(&o, &m, &f, &cache, &trees, steam, brave)?;

    // The stick once more: still the same disk.
    if o.image.is_none() {
        let again = devs.list()?;
        if !again.iter().any(|d| d.same_as(&disk)) {
            return Err("refused: the stick changed while the installer was preparing (unplugged or swapped). Nothing was written.".into());
        }
    }
    let mut log = |s: &str| println!("{s}");
    // The host's record first: a token that only the stick knows would be lost to its owner.
    let host_dir = o.host_dir.clone().or_else(secrets::default_host_dir);
    let (stick_id, cfg) = match &ans {
        Some(a) => (secrets::new_stick_id()?, Some(config::render(a, release::VERSION)?)),
        None => (bitsos.and_then(|e| e.stick_id.clone()).expect("checked above"), None),
    };
    let token = ans.as_ref().and_then(|a| a.token.clone());
    let saved = match &host_dir {
        Some(h) => Some(secrets::save(h, &stick_id, token.as_deref()).map_err(|e| format!("{e}\nThis computer could not keep the stick's ID{}; --host-dir picks another folder. Nothing was written.", if token.is_some() { " and token" } else { "" }))?),
        None if token.is_some() => return Err("remote control needs a folder on this computer for its token (no home folder found): give --host-dir. Nothing was written.".into()),
        None => None,
    };
    let done = match cfg {
        Some(cfg) => stick::fresh(dev.as_mut(), &plan, &stick_id, &cfg, &mut log)?,
        None => stick::update(dev.as_mut(), &plan, &mut log)?,
    };
    drop(dev);
    notes.extend(done.notes.iter().cloned());
    finish(&done, &notes, &missing, saved.as_deref(), token.as_deref(), o.batch());
    Ok(())
}

fn finish(done: &stick::Done, notes: &[String], missing: &[String], saved: Option<&std::path::Path>, token: Option<&str>, batch: bool) {
    println!("\nDone: {} files ({} MB) written and read back; generations: {}.", done.files, done.bytes >> 20, done.sys);
    for n in notes {
        println!("note: {n}");
    }
    if !missing.is_empty() {
        println!("Not on the stick: {}", missing.iter().map(|m| m.split(':').next().unwrap_or("")).collect::<Vec<_>>().join(", "));
    }
    if let Some(d) = saved {
        println!("This computer keeps the stick's ID{} in {}.", if token.is_some() { " and its remote-control token" } else { "" }, d.display());
    }
    if let Some(t) = token {
        // Without --erase/--update a person reads this; with them an agent's transcript would keep it.
        if batch {
            println!("\nThe remote-control token is in token.txt there (not printed in this mode).");
        } else {
            println!("\nRemote-control token (shown this once; it is also in token.txt there):\n  {t}");
        }
        println!("Pair your computer after the first start: the BitsOS screen shows a line `bitsmcp: TLS server key sha256 <hex>`.");
        println!("  With host/mcp/ from {} (Node.js):", release::REPOSITORY);
        println!("  node host/mcp/pair.mjs <BitsOS address>:8765 --expect <hex> --token-file <that token.txt>");
        println!("  and answer the question at the BitsOS machine's keyboard.");
    }
    println!("\nNext:");
    println!("  1. Turn Secure Boot off in the firmware settings (bitsboot is not signed).");
    println!("  2. Plug the stick in, switch on and open the firmware's boot menu (often F7, F8, F11, F12 or Esc);");
    println!("     choose the stick's UEFI entry.");
    println!("  3. BitsOS has a German (QWERTZ) keyboard layout only: on a US keyboard Y and Z are swapped and");
    println!("     some symbols sit elsewhere.");
    println!("  4. Sound over HDMI can be loud: turn the screen's or amplifier's volume down before the first start.");
    println!("  If nothing starts: README section \"Trying BitsOS\" lists what to check.");
}
