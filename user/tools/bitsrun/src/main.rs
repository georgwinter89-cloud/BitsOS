//! The `bitsrun` command (PK-3): `bitsrun [--store DIR] [--version V] [--plan] NAME [ARGS...]` starts an installed
//! program in its view (`--exec PROG`: another program of its tree in that view, such as a second .desktop file's
//! Exec); `bitsrun --root TREE [--name N] [--data DIR] [--plan] -- CMD [ARGS...]` starts CMD in the
//! view of a bare root tree (tests).
#![forbid(unsafe_code)]

use bitspkg::names::{check_name, check_version};
use bitspkg::perm::Permission;
use bitspkg::store::Store;
use bitsrun::facts::{self, FsTree};
use bitsrun::plan::{self, Input, Kind, Tree};
use bitsrun::{bwrap, event};
use std::ffi::OsString;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, exit};

const USAGE: &str = "usage: bitsrun [--store DIR] [--version V] [--exec PROG] [--plan] NAME [ARGS...]
       bitsrun --root TREE [--name N] [--data DIR] [--no-net] [--plan] -- CMD [ARGS...]
  NAME      an installed program (/apps/NAME/current, or --version V); --store DIR: the store root (default /)
  --exec    (with NAME) start PROG instead of the package's program: absolute in the view, or a name in /usr/bin
  --root    a program root tree (TREE/usr, TREE/etc); CMD absolute in the view, or a name in /usr/bin
  --no-net  (with --root) a network namespace of its own and none of the system's fonts: the tree alone
            (bitspkg's triggers, PK-6d)
  --plan    print the view (binds in order, environment, notes) and start nothing";

#[derive(Default)]
struct Opts {
    store: Option<PathBuf>,
    version: Option<String>,
    root: Option<PathBuf>,
    name: Option<String>,
    data: Option<PathBuf>,
    /// `--exec`: the program of NAME's tree to start instead of the manifest's (the desktop's .desktop entries).
    exec: Option<String>,
    no_net: bool,
    plan: bool,
    /// NAME or CMD, then the program's arguments as given (a file name need not be UTF-8).
    rest: Vec<OsString>,
}

/// A word bitsrun itself reads (an option's value, NAME, CMD) must be UTF-8; the program's arguments need not.
fn utf8(a: OsString, what: &str) -> Result<String, String> {
    a.into_string().map_err(|a| format!("{what} {:?} ist kein UTF-8", a.to_string_lossy()))
}

fn parse(args: Vec<OsString>) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut val = |what: &str| utf8(it.next().ok_or_else(|| format!("{what} braucht einen Wert"))?, what);
        match a.to_str().unwrap_or("") {
            "--store" => o.store = Some(val("--store")?.into()),
            "--version" => o.version = Some(val("--version")?),
            "--root" => o.root = Some(val("--root")?.into()),
            "--name" => o.name = Some(val("--name")?),
            "--data" => o.data = Some(val("--data")?.into()),
            "--exec" => o.exec = Some(val("--exec")?),
            "--plan" => o.plan = true,
            "--no-net" => o.no_net = true,
            "--" => {
                o.rest = it.collect();
                break;
            }
            "-h" | "--help" => return Err(USAGE.into()),
            s if s.starts_with("--") => return Err(format!("unbekannte Option {s}\n{USAGE}")),
            _ => {
                o.rest = std::iter::once(a).chain(it).collect();
                break;
            }
        }
    }
    if o.rest.is_empty() {
        return Err(USAGE.into());
    }
    Ok(o)
}

fn abs(p: &Path) -> Result<String, String> {
    let c = p.canonicalize().map_err(|e| format!("{}: {e}", p.display()))?;
    c.into_os_string().into_string().map_err(|_| format!("{}: Pfad ist kein UTF-8", p.display()))
}

/// The data folder must belong to the caller (the program's uid): bitsrun never runs as root, so it cannot give
/// it away; a missing one is made (0700) as install makes it.
fn data_folder(d: &Path, uid: u32) -> Result<String, String> {
    if !d.exists() {
        bitspkg::fsx::mkdir(d, 0o700)?;
        event("pkg.run.data.created", &[("path", &d.to_string_lossy())]);
    }
    let m = std::fs::symlink_metadata(d).map_err(|e| format!("{}: {e}", d.display()))?;
    if !m.is_dir() {
        return Err(format!("{}: der Datenordner ist kein Ordner", d.display()));
    }
    if m.uid() != uid {
        return Err(format!(
            "{}: der Datenordner gehört uid {}, das Programm läuft als uid {uid}; als root: chown {uid}:{uid} {}",
            d.display(),
            m.uid(),
            d.display()
        ));
    }
    abs(d)
}

struct Target {
    tree_dir: String,
    name: Option<String>,
    version: Option<String>,
    data: Option<String>,
    perms: Vec<Permission>,
    program: String,
    args: Vec<OsString>,
    id: Option<u64>,
}

fn installed(o: &Opts, uid: u32) -> Result<Target, String> {
    let st = Store::new(o.store.as_deref().unwrap_or(Path::new("/")));
    let name = &utf8(o.rest[0].clone(), "Programmname")?;
    check_name(name).map_err(|e| format!("Programmname: {e}"))?;
    let v = match &o.version {
        Some(v) => {
            if v.contains(':') {
                // pacman's epoch ("1:2.3-1") is no folder name; the store's folder for it is PK-6d's mapping.
                return Err(format!(
                    "Version {v}: eine Epoche (\"1:\") ist kein Ordnername; --version nimmt den Ordnernamen unter {}/",
                    st.pkg(name).display()
                ));
            }
            check_version(v).map_err(|e| format!("Version: {e}"))?;
            v.clone()
        }
        // This line reaches the desktop's card (PE-1), so no path: the event line names the package (CR-PE1-8).
        None => st.current(name).ok_or_else(|| format!("{name} ist nicht installiert"))?,
    };
    let vd = st.version_dir(name, &v);
    if !vd.is_dir() {
        return Err(format!("{name} {v} ist nicht installiert"));
    }
    let m = st.manifest(name, &v)?;
    // D8: the per-program root tree; a PK-4 view-layout package (usr/ at the top) is its own tree.
    let root_tree = vd.join("root").is_dir();
    let tree = if root_tree { vd.join("root") } else { vd.clone() };
    let program = match &o.exec {
        Some(e) => exec_in_view(e)?,
        None => {
            let prog = m.program.clone().ok_or_else(|| format!("{name} {v}: das Paket nennt kein Programm (program = ...)"))?;
            plan::program_in_view(&prog, root_tree)?
        }
    };
    Ok(Target {
        tree_dir: abs(&tree)?,
        name: Some(name.clone()),
        version: Some(v),
        data: Some(data_folder(&st.data_dir(name), uid)?),
        perms: m.permissions.clone(),
        program,
        args: o.rest[1..].to_vec(),
        id: st.program_id(name),
    })
}

/// `--exec PROG` as a path in the view: an absolute one as given, a bare name in /usr/bin; the same rule as the
/// manifest's program (inside usr/, opt/ or srv/ of the tree, no `..`).
fn exec_in_view(e: &str) -> Result<String, String> {
    let rel = match e.strip_prefix('/') {
        Some(r) => r.to_string(),
        None if !e.contains('/') => format!("usr/bin/{e}"),
        None => return Err(format!("--exec {e:?}: ein absoluter Pfad in der Sicht oder ein Name in /usr/bin")),
    };
    plan::program_in_view(&rel, false).map_err(|err| format!("--exec: {err}"))
}

fn bare(o: &Opts, root: &Path, uid: u32) -> Result<Target, String> {
    if let Some(n) = &o.name {
        check_name(n).map_err(|e| format!("--name: {e}"))?;
    }
    let cmd = &utf8(o.rest[0].clone(), "Befehl")?;
    let program = if cmd.starts_with('/') { cmd.clone() } else { format!("/usr/bin/{cmd}") };
    plan::program_in_view(program.trim_start_matches('/'), false)?;
    Ok(Target {
        tree_dir: abs(root)?,
        name: o.name.clone(),
        version: None,
        data: match (&o.name, &o.data) {
            (Some(_), Some(d)) => Some(data_folder(d, uid)?),
            (None, Some(_)) => return Err("--data braucht --name".into()),
            _ => None,
        },
        perms: Vec::new(),
        program,
        args: o.rest[1..].to_vec(),
        id: None,
    })
}

fn run(o: Opts) -> Result<(), String> {
    let uid = usys::getuid().map_err(|e| format!("getuid: {e}"))? as u32;
    if usys::geteuid().map(|u| u == 0).unwrap_or(true) {
        return Err("bitsrun startet keine Programme als root: als Benutzer (uid 1000) aufrufen".into());
    }
    let t = match &o.root {
        Some(_) if o.exec.is_some() => return Err("--exec gilt nur für ein installiertes Programm (NAME), nicht mit --root".into()),
        Some(r) => bare(&o, r, uid)?,
        None => installed(&o, uid)?,
    };
    let pw = facts::passwd(Path::new("/etc/passwd"), uid);
    let home = facts::home(std::env::var("HOME").ok().as_deref(), pw.as_ref(), uid);
    let user = pw.as_ref().map(|(n, _)| n.clone()).unwrap_or_else(|| format!("u{uid}"));
    let tree = FsTree(PathBuf::from(&t.tree_dir));
    // Only names and values bitsrun can read are looked at; ENV_PASS's are plain words, others never pass.
    let mut skipped = Vec::new();
    let env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| match (k.into_string(), v.into_string()) {
            (Ok(k), Ok(v)) => Some((k, v)),
            (k, _) => {
                skipped.push(k.unwrap_or_else(|k| k.to_string_lossy().into_owned()));
                None
            }
        })
        .collect();
    // BR-9: a browser from the repositories starts as the stick's launcher starts Brave (before the facts: the
    // downloads folder it may make is one of them).
    let browser = match (&o.root, &t.data) {
        (None, Some(d)) => {
            let stick = Store::new(o.store.as_deref().unwrap_or(Path::new("/"))).data_dir(bitsrun::browser::BRAVE.stick_data);
            bitsrun::browser_run::prepare(&tree, &t.program, Path::new(d), &stick, &home, uid, o.plan)?
        }
        _ => None,
    };
    let mut system = facts::system(Path::new("/etc"), uid, &home);
    let no_fonts = system.fonts.is_none() && !o.no_net;
    if o.no_net {
        // A trigger's output is hashed in the manifest and shared in the store: it comes from the tree alone, so
        // its view has neither the network nor the system's fonts.
        system.fonts = None;
    }
    let input = Input {
        tree: &tree,
        tree_dir: t.tree_dir.clone(),
        name: t.name.clone(),
        data: t.data.clone(),
        home: home.clone(),
        user,
        system,
        permissions: &t.perms,
        env,
        program: t.program.clone(),
        args: t.args.clone(),
    };
    let mut p = plan::build(&input)?;
    if let Some(b) = browser {
        b.apply(&mut p);
    }
    if o.no_net {
        if o.root.is_none() {
            return Err("--no-net gehört zu --root".into());
        }
        p.ops.insert(0, plan::Op::UnshareNet);
    }
    if no_fonts {
        p.notes.push(format!("Schriften: das System hat keinen Ordner {}; das Programm sieht nur die Schriften seines Baums", plan::SYSTEM_FONTS));
    }
    if !skipped.is_empty() {
        p.notes.push(format!("Umgebung: Einträge ohne UTF-8 weggelassen: {}", skipped.join(", ")));
    }
    // ProgramId (PACKAGES.md 8): the kernel sets it only at its own spawn; there is no call for a launcher yet
    // (open item in plan/notes/desk-offen.md, group G3). Until then the id is reported, not set.
    let id = t.id.map(|i| i.to_string()).unwrap_or_else(|| "-".into());
    if o.plan {
        println!("tree {}", t.tree_dir);
        println!("program_id {id} (nicht gesetzt: der Kern hat noch keinen Weg dafür)");
        for op in &p.ops {
            println!("{op}");
        }
        for (k, v) in &p.env {
            println!("env {k}={v}");
        }
        println!("chdir {}", p.chdir);
        println!("run {}", p.argv.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" "));
        for n in &p.notes {
            println!("note {n}");
        }
        return Ok(());
    }
    if tree.kind("usr/lib/ld-linux-x86-64.so.2") == Kind::Missing {
        event("pkg.run.no_loader", &[("tree", &t.tree_dir)]);
    }
    let name = t.name.clone().unwrap_or_default();
    let ver = t.version.clone().unwrap_or_default();
    event("pkg.run.program_id", &[("name", &name), ("id", &id), ("set", "no"), ("reason", "no kernel call yet (CR AB)")]);
    for n in &p.notes {
        event("pkg.run.note", &[("name", &name), ("note", n)]);
    }
    event(
        "pkg.run.start",
        &[("name", &name), ("version", &ver), ("program", &t.program), ("ops", &p.ops.len().to_string()), ("via", bwrap::BWRAP)],
    );
    let e = Command::new(bwrap::BWRAP).args(bwrap::args(&p)).env_clear().envs(p.env.iter().cloned()).exec();
    Err(format!("{} lässt sich nicht starten: {e}", bwrap::BWRAP))
}

fn main() {
    let o = match parse(std::env::args_os().skip(1).collect()) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            exit(2);
        }
    };
    if let Err(e) = run(o) {
        event("pkg.run.refused", &[("reason", &e)]);
        // The person at the screen reads this line on the desktop's card (PE-1: "» " marks it); the event line
        // above is the desk's.
        eprintln!("» {e}");
        exit(1);
    }
}
