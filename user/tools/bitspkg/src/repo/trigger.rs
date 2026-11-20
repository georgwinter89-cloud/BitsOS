//! alpm hooks emulated as sandboxed triggers (plan 1.7). A hook of the closure (its `usr/share/libalpm/hooks/`)
//! that an install would set off (alpmhook.rs) is looked up in two tables: emulated ones run the closure's own
//! tool once, inside the version's staging view through `bitsrun --root <staging>/root --no-net` (PK-3), as the
//! program uid, with write access only to a fresh output folder (bitsrun's data folder); the output file is then
//! put into the tree by the caller and hashed in the manifest. Refused ones are listed with their reason; any
//! other matching hook is reported "nicht ausgeführt". pacman would run the hook's Exec as root in the real
//! system (hook.c:507 _alpm_hook_run_hook through _alpm_run_chroot); BitsOS never does.
//! The seccomp profile of SX-2 is not applied yet: bitsrun has no way to load one (open item in
//! plan/notes/desk-offen.md, group G3); until then a trigger has the view, its own network namespace and Unix rights.

use super::alpmhook::{self, Hook};
use super::pkgs::List;
use super::refresh::event;
use crate::fsx::{self, err};
use std::fs::{self, File};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// bitsrun's `--name` for the trigger runs: its data folder is the output folder (`/data/appdata/trigger`).
pub const NAME: &str = "trigger";
const VIEW_OUT: &str = "/data/appdata/trigger";
/// One tool run: a locale is compiled in seconds, a large icon theme in a minute.
const RUN_MAX: Duration = Duration::from_secs(300);
/// What one run may write (its stdout, stderr and output folder together) at least; a closure larger than this
/// may write as much as the closure holds (a cache is never larger than its input). Checked once a second.
const OUT_MIN: u64 = 512 << 20;
/// The locales locale-gen makes (plan 1.7): German and English.
pub const LOCALES: [(&str, &str); 2] = [("de_DE", "de_DE.utf8"), ("en_US", "en_US.utf8")];

/// Hooks refused on purpose (plan 1.7's table): file-name glob, reason.
pub const REFUSED: [(&str, &str); 24] = [
    ("*lib32*", "32-Bit-Teil: multilib ist aus (D3), ein Programmbaum hat kein lib32"),
    ("*-32", "32-Bit-Teil: multilib ist aus (D3), ein Programmbaum hat kein lib32"),
    ("20-systemd-sysusers", "keine Systembenutzer in BitsOS: die Dateien laufen als uid des Programms"),
    ("21-systemd-tmpfiles", "keine tmpfiles in BitsOS: das Programm schreibt nur in seinen Datenordner"),
    ("25-systemd-*", "BitsOS hat kein systemd als Init"),
    ("30-systemd-daemon-reload-*", "BitsOS hat kein systemd als Init"),
    ("35-systemd-*", "BitsOS hat kein systemd als Init"),
    ("36-systemd-modules-load", "BitsOS hat kein systemd als Init"),
    ("60-depmod", "Kern-Seite: BitsOS bringt Kern und Module selbst mit"),
    ("*mkinitcpio*", "Kern-Seite: BitsOS bringt seinen Start selbst mit"),
    ("cachyos-plymouth-initramfs", "Kern-Seite: BitsOS bringt seinen Start selbst mit"),
    ("40-update-ca-trust", "BitsOS bindet sein eigenes CA-Bündel in jede Sicht"),
    ("dbus-reload", "betrifft das System, nicht den Programmbaum"),
    ("accounts-daemon-restart", "betrifft das System, nicht den Programmbaum"),
    ("*-openssh-*", "betrifft das System, nicht den Programmbaum"),
    ("cachyos-reboot-required", "betrifft das System, nicht den Programmbaum"),
    ("cachyos-rate-mirrors", "keine Geschwindigkeitstests gegen fremde Server (plan 1.2)"),
    ("rebuild-detector", "betrifft das System, nicht den Programmbaum"),
    ("os-release", "betrifft das System, nicht den Programmbaum"),
    ("lsb-release", "betrifft das System, nicht den Programmbaum"),
    ("cachyos-branding", "betrifft das System, nicht den Programmbaum"),
    ("texinfo-*", "im Programmbaum ohne Nutzen"),
    ("man-db-remove-cache", "im Programmbaum ohne Nutzen"),
    ("*-appstream-cache", "im Programmbaum ohne Nutzen"),
];
/// More of the table that a glob above does not say as plainly.
pub const REFUSED_MORE: [(&str, &str); 2] = [("vimdoc", "im Programmbaum ohne Nutzen"), ("detect-old-perl-modules", "im Programmbaum ohne Nutzen")];

/// The emulated hooks: hook name -> what is done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Emu {
    Ldconfig,
    Iconv,
    LocaleGen,
    Schemas,
    GioModules,
    Gtk4Modules,
    PixbufLoaders,
    GtkImmodules3,
    MimeDb,
    DesktopDb,
    IconCache,
    /// 40-fontconfig-config: links in etc/fonts/conf.d (done by the tree, no tool).
    FontconfigConfig,
    /// fontconfig.hook (fc-cache): at the program's first start, into its data folder.
    FcCacheFirstStart,
}

/// The tool an emulation runs (in the view): a closure without it gets "nicht ausgeführt", as pacman skips a hook
/// whose Depends are not installed (hook.c:511). The hooks' own Depends name what their Exec needs (bash for
/// locale-gen's script), which an emulation does not run, so the emulation's tool is what is checked.
fn tool(e: Emu) -> Option<&'static str> {
    Some(match e {
        Emu::Ldconfig => "/usr/bin/ldconfig",
        Emu::Iconv => "/usr/bin/iconvconfig",
        Emu::LocaleGen => "/usr/bin/localedef",
        Emu::Schemas => "/usr/bin/glib-compile-schemas",
        Emu::GioModules | Emu::Gtk4Modules => "/usr/bin/gio-querymodules",
        Emu::PixbufLoaders => "/usr/bin/gdk-pixbuf-query-loaders",
        Emu::GtkImmodules3 => "/usr/bin/gtk-query-immodules-3.0",
        Emu::MimeDb => "/usr/bin/update-mime-database",
        Emu::DesktopDb => "/usr/bin/update-desktop-database",
        Emu::IconCache => "/usr/bin/gtk-update-icon-cache",
        Emu::FontconfigConfig | Emu::FcCacheFirstStart => return None,
    })
}

/// `rel` below `root` with no symlink on the way: the root tree's links are the packages', and a root-side read
/// must never follow one out of the tree (`usr/share/glib-2.0/schemas -> /etc`). `last_link`: the last component
/// may be a link (only its existence is asked).
fn plain(root: &Path, rel: &str, last_link: bool) -> Option<PathBuf> {
    let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
    let mut p = root.to_path_buf();
    for (i, c) in comps.iter().enumerate() {
        if matches!(*c, "." | "..") {
            return None;
        }
        p.push(c);
        let m = fs::symlink_metadata(&p).ok()?;
        let last = i + 1 == comps.len();
        if (m.file_type().is_symlink() && !(last && last_link)) || (!last && !m.is_dir()) {
            return None;
        }
    }
    Some(p)
}

/// Bytes of the regular files below `d` (never through a link), for the output cap.
fn bytes_below(d: &Path) -> u64 {
    let mut sum = 0u64;
    let mut todo = vec![d.to_path_buf()];
    while let Some(p) = todo.pop() {
        let Ok(rd) = fs::read_dir(&p) else { continue };
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => todo.push(e.path()),
                Ok(t) if t.is_file() => sum = sum.saturating_add(e.metadata().map(|m| m.len()).unwrap_or(0)),
                _ => {}
            }
        }
    }
    sum
}

pub const EMULATED: [(&str, Emu); 13] = [
    ("11-glibc-ldconfig", Emu::Ldconfig),
    ("12-glibc-iconvconfig", Emu::Iconv),
    ("10-glibc-locale-gen", Emu::LocaleGen),
    ("glib-compile-schemas", Emu::Schemas),
    ("gio-querymodules", Emu::GioModules),
    ("gtk4-querymodules", Emu::Gtk4Modules),
    ("gdk-pixbuf-query-loaders", Emu::PixbufLoaders),
    ("gtk-query-immodules-3.0", Emu::GtkImmodules3),
    ("30-update-mime-database", Emu::MimeDb),
    ("update-desktop-database", Emu::DesktopDb),
    ("gtk-update-icon-cache", Emu::IconCache),
    ("40-fontconfig-config", Emu::FontconfigConfig),
    ("fontconfig", Emu::FcCacheFirstStart),
];

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// The tool ran; its outputs (path in the root tree, file in the output folder).
    Emulated(Vec<(String, PathBuf)>),
    /// Links the tree makes itself: (link in the root tree, target in the view).
    Links(Vec<(String, String)>),
    Later(String),
    Refused(String),
    Failed(String),
    NotRun(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Result1 {
    pub hook: String,
    pub tool: String,
    pub targets: Vec<String>,
    pub outcome: Outcome,
}

/// The closure's hooks: every `usr/share/libalpm/hooks/*.hook` of its packages, read from the staging tree.
pub fn hooks(root: &Path, closure: &[(String, &List)]) -> Vec<Result<Hook, String>> {
    let mut v = Vec::new();
    for (_, l) in closure {
        for f in &l.files {
            if let Some(n) = f.path.strip_prefix(alpmhook::HOOK_DIR).and_then(|n| n.strip_suffix(".hook"))
                && !n.contains('/')
            {
                let p = root.join(&f.path);
                let r = match fs::symlink_metadata(&p) {
                    Ok(m) if m.is_file() && m.len() <= alpmhook::HOOK_MAX as u64 => {
                        fs::read_to_string(&p).map_err(|e| err(&p, e)).and_then(|t| alpmhook::parse(n, &t))
                    }
                    _ => Err(format!("{n}.hook: keine Datei oder zu groß")),
                };
                v.push(r);
            }
        }
    }
    // hook.c sorts by file name without the suffix.
    v.sort_by(|a, b| a.as_ref().map(|h| h.name.clone()).unwrap_or_default().cmp(&b.as_ref().map(|h| h.name.clone()).unwrap_or_default()));
    v
}

fn refused(name: &str) -> Option<&'static str> {
    REFUSED.iter().chain(REFUSED_MORE.iter()).find(|(g, _)| alpmhook::fnmatch(g.as_bytes(), name.as_bytes())).map(|(_, r)| *r)
}

pub struct Runner<'a> {
    /// The staging root tree (`apps/<prog>/.new-<v>/root`).
    pub root: &'a Path,
    /// A work folder beside it (removed before the version is sealed).
    pub work: &'a Path,
    pub bitsrun: PathBuf,
    /// Drop to this uid/gid for the runs (bitsrun never runs as root).
    pub run_as: Option<(u32, u32)>,
    /// What one run may write (OUT_MIN, or the closure's size when larger: run_all sets it).
    pub out_max: u64,
    seq: u32,
}

impl<'a> Runner<'a> {
    pub fn new(root: &'a Path, work: &'a Path) -> Runner<'a> {
        let bitsrun = std::env::var_os("BITSPKG_BITSRUN").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/bin/bitsrun"));
        let uid = crate::clean::PROGRAM_UID;
        let root_user = usys::geteuid().map(|u| u == 0).unwrap_or(false);
        Runner { root, work, bitsrun, run_as: root_user.then_some((uid, uid)), out_max: OUT_MIN, seq: 0 }
    }

    fn has(&self, rel: &str) -> bool {
        plain(self.root, rel, true).is_some()
    }

    fn dir(&self, rel: &str) -> Option<PathBuf> {
        plain(self.root, rel, false).filter(|p| p.is_dir())
    }

    /// A fresh output folder for one run, owned by the run's uid (bitsrun checks the data folder's owner).
    fn out_dir(&mut self) -> Result<PathBuf, String> {
        self.seq += 1;
        let d = self.work.join(format!("out-{}", self.seq));
        fsx::remove_tree(&d)?;
        fsx::mkdir(&d, 0o700)?;
        Ok(d)
    }

    fn give(&self, d: &Path) -> Result<(), String> {
        let Some((u, g)) = self.run_as else { return Ok(()) };
        // Folders only: a mirrored file is a hard link to a store inode, which must stay root's (0444).
        let mut todo = vec![d.to_path_buf()];
        while let Some(p) = todo.pop() {
            if fs::symlink_metadata(&p).map(|m| m.is_dir()).unwrap_or(false) {
                std::os::unix::fs::lchown(&p, Some(u), Some(g)).map_err(|e| err(&p, e))?;
                todo.extend(fs::read_dir(&p).map_err(|e| err(&p, e))?.flatten().map(|e| e.path()));
            }
        }
        Ok(())
    }

    /// A copy of the root tree's folder `rel` in `out/<as>`: folders made, files hard-linked (copied where a link
    /// is refused), links recreated: tools that write their cache beside their input (gio-querymodules,
    /// gtk-update-icon-cache) write it there, never into the tree.
    fn mirror(&self, rel: &str, out: &Path, as_: &str, only_ext: Option<&str>) -> Result<(), String> {
        let src = self.dir(rel).ok_or_else(|| format!("root/{rel}: kein Ordner oder ein Verweis auf dem Weg: nicht ausgeführt"))?;
        let dst = out.join(as_);
        let mut todo = vec![(src, dst)];
        while let Some((s, d)) = todo.pop() {
            fsx::mkdir(&d, 0o755)?;
            for e in fs::read_dir(&s).map_err(|x| err(&s, x))?.flatten() {
                let ft = e.file_type().map_err(|x| err(&e.path(), x))?;
                let to = d.join(e.file_name());
                if ft.is_dir() {
                    if only_ext.is_none() {
                        todo.push((e.path(), to));
                    }
                } else if ft.is_symlink() {
                    if let Ok(t) = fs::read_link(e.path()) {
                        std::os::unix::fs::symlink(t, &to).map_err(|x| err(&to, x))?;
                    }
                } else if ft.is_file() {
                    if only_ext.is_some_and(|x| !e.file_name().to_string_lossy().ends_with(x)) {
                        continue;
                    }
                    if fs::hard_link(e.path(), &to).is_err() {
                        fs::copy(e.path(), &to).map_err(|x| err(&to, x))?;
                    }
                }
            }
        }
        Ok(())
    }

    /// The UTF-8 charmap for localedef, unpacked into `out` (bounded: the real one is 300 KiB unpacked); its path
    /// in the view.
    fn charmap(&self, out: &Path) -> Result<String, String> {
        let file = |n: &str| plain(self.root, &format!("usr/share/i18n/charmaps/{n}"), false).filter(|p| p.is_file());
        let (src, gz) = match (file("UTF-8.gz"), file("UTF-8")) {
            (Some(p), _) => (p, true),
            (None, Some(p)) => (p, false),
            (None, None) => return Err("root/usr/share/i18n/charmaps/UTF-8(.gz): keine Datei oder ein Verweis auf dem Weg".into()),
        };
        let f = File::open(&src).map_err(|e| err(&src, e))?;
        let mut r: Box<dyn std::io::Read> = if gz {
            super::unpack::open(super::unpack::Bounded::new(f, 16 << 20, "charmap")).map_err(|e| err(&src, e))?
        } else {
            Box::new(super::unpack::Bounded::new(f, 16 << 20, "charmap"))
        };
        let dst = out.join("UTF-8");
        let mut o = File::create(&dst).map_err(|e| err(&dst, e))?;
        std::io::copy(&mut r, &mut o).map_err(|e| err(&dst, e))?;
        Ok(format!("{VIEW_OUT}/UTF-8"))
    }

    /// One tool run in the staging view; Ok(stdout file) when it ended with a status in `ok`.
    fn run(&mut self, out: &Path, tool: &str, args: &[&str], ok: &[i32]) -> Result<PathBuf, String> {
        self.give(out)?;
        let n = self.seq;
        let so = self.work.join(format!("stdout-{n}"));
        let se = self.work.join(format!("stderr-{n}"));
        let mut c = Command::new(&self.bitsrun);
        c.arg("--root").arg(self.root).arg("--name").arg(NAME).arg("--data").arg(out).arg("--no-net").arg("--").arg(tool).args(args);
        c.env_clear().env("PATH", "/usr/bin:/bin").env("LANG", "C").stdin(Stdio::null());
        c.stdout(File::create(&so).map_err(|e| err(&so, e))?).stderr(File::create(&se).map_err(|e| err(&se, e))?);
        if let Some((u, g)) = self.run_as {
            c.uid(u).gid(g);
        }
        let before = bytes_below(out);
        let mut child = c.spawn().map_err(|e| format!("{} startet nicht: {e}", self.bitsrun.display()))?;
        let deadline = Instant::now() + RUN_MAX;
        let mut next_size = Instant::now() + Duration::from_secs(1);
        let st = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) if Instant::now() >= next_size => {
                    next_size = Instant::now() + Duration::from_secs(1);
                    let size = |p: &Path| fs::symlink_metadata(p).map(|m| m.len()).unwrap_or(0);
                    let wrote = (bytes_below(out).saturating_sub(before)).saturating_add(size(&so)).saturating_add(size(&se));
                    if wrote > self.out_max {
                        let _ = child.kill();
                        let _ = child.wait();
                        event("pkg.trigger.too_much", &[("tool", tool), ("bytes", &wrote.to_string())]);
                        return Err(format!("{tool} schrieb mehr als {} MiB und wurde beendet", self.out_max >> 20));
                    }
                }
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("{tool} lief länger als {} s und wurde beendet", RUN_MAX.as_secs()));
                }
                Err(e) => return Err(format!("{tool}: {e}")),
            }
        };
        let code = st.code().unwrap_or(-1);
        if !ok.contains(&code) {
            let tail = super::download::read_small(&se, 1 << 20).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
            let last: Vec<&str> = tail.lines().filter(|l| !l.trim().is_empty()).rev().take(2).collect();
            return Err(format!("{tool} endete mit {code}: {}", last.into_iter().rev().collect::<Vec<_>>().join(" / ")));
        }
        Ok(so)
    }

    /// The regular file `p` the tool made (never a link it left there).
    fn made(p: &Path) -> Result<PathBuf, String> {
        match fs::symlink_metadata(p) {
            Ok(m) if m.is_file() && m.nlink() == 1 => Ok(p.to_path_buf()),
            Ok(_) => Err(err(p, "keine gewöhnliche Datei")),
            Err(e) => Err(err(p, format!("nicht erzeugt ({e})"))),
        }
    }

    /// Every regular file below `d` (relative names), for tools that write a whole folder.
    fn files_below(d: &Path, skip: &[&str]) -> Result<Vec<(String, PathBuf)>, String> {
        let mut out = Vec::new();
        let mut todo = vec![(d.to_path_buf(), String::new())];
        while let Some((p, rel)) = todo.pop() {
            for e in fs::read_dir(&p).map_err(|x| err(&p, x))?.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let r = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
                if skip.contains(&r.as_str()) {
                    continue;
                }
                let ft = e.file_type().map_err(|x| err(&e.path(), x))?;
                if ft.is_dir() {
                    todo.push((e.path(), r));
                } else if ft.is_file() {
                    out.push((r, e.path()));
                }
            }
        }
        out.sort();
        Ok(out)
    }

    fn emulate(&mut self, emu: Emu, targets: &[String]) -> Result<(String, Outcome), String> {
        let o = Outcome::Emulated;
        Ok(match emu {
            Emu::Ldconfig => {
                let out = self.out_dir()?;
                self.run(&out, "/usr/bin/ldconfig", &["-X", "-i", "-C", &format!("{VIEW_OUT}/ld.so.cache")], &[0])?;
                ("ldconfig".into(), o(vec![("etc/ld.so.cache".into(), Self::made(&out.join("ld.so.cache"))?)]))
            }
            Emu::Iconv => {
                let out = self.out_dir()?;
                self.run(&out, "/usr/bin/iconvconfig", &["-o", &format!("{VIEW_OUT}/gconv-modules.cache")], &[0])?;
                ("iconvconfig".into(), o(vec![("usr/lib/gconv/gconv-modules.cache".into(), Self::made(&out.join("gconv-modules.cache"))?)]))
            }
            Emu::LocaleGen => {
                let mut v = Vec::new();
                for (src, name) in LOCALES {
                    if !self.has(&format!("usr/share/i18n/locales/{src}")) {
                        event("pkg.trigger.locale_missing", &[("locale", src)]);
                        continue;
                    }
                    let out = self.out_dir()?;
                    // glibc's charmaps are gzip'd and localedef reads them through `gzip -dc`, which a closure
                    // need not have: the charmap is unpacked here (miniz_oxide) into the output folder.
                    let cm = self.charmap(&out)?;
                    // localedef: exit 1 = output made with warnings (-c), as locale-gen accepts it.
                    self.run(&out, "/usr/bin/localedef", &["--no-archive", "-c", "-i", src, "-f", &cm, &format!("{VIEW_OUT}/{name}")], &[0, 1])?;
                    for (r, p) in Self::files_below(&out.join(name), &[])? {
                        v.push((format!("usr/lib/locale/{name}/{r}"), p));
                    }
                }
                if v.is_empty() {
                    return Err("keine Sprachdaten (usr/share/i18n/locales) im Baum".into());
                }
                ("localedef".into(), o(v))
            }
            Emu::Schemas => {
                let out = self.out_dir()?;
                self.run(&out, "/usr/bin/glib-compile-schemas", &[&format!("--targetdir={VIEW_OUT}"), "/usr/share/glib-2.0/schemas"], &[0])?;
                ("glib-compile-schemas".into(), o(vec![("usr/share/glib-2.0/schemas/gschemas.compiled".into(), Self::made(&out.join("gschemas.compiled"))?)]))
            }
            Emu::GioModules | Emu::Gtk4Modules => {
                let dirs: Vec<String> = if emu == Emu::GioModules {
                    vec!["usr/lib/gio/modules".into()]
                } else {
                    targets.iter().map(|t| t.trim_end_matches('/').to_string()).filter(|d| self.dir(d).is_some()).collect()
                };
                let mut v = Vec::new();
                for d in dirs {
                    let out = self.out_dir()?;
                    self.mirror(&d, &out, "m", None)?;
                    self.run(&out, "/usr/bin/gio-querymodules", &[&format!("{VIEW_OUT}/m")], &[0])?;
                    v.push((format!("{d}/giomodule.cache"), Self::made(&out.join("m/giomodule.cache"))?));
                }
                ("gio-querymodules".into(), o(v))
            }
            Emu::PixbufLoaders => {
                let out = self.out_dir()?;
                let so = self.run(&out, "/usr/bin/gdk-pixbuf-query-loaders", &[], &[0])?;
                ("gdk-pixbuf-query-loaders".into(), o(vec![("usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache".into(), so)]))
            }
            Emu::GtkImmodules3 => {
                let out = self.out_dir()?;
                let so = self.run(&out, "/usr/bin/gtk-query-immodules-3.0", &[], &[0])?;
                ("gtk-query-immodules-3.0".into(), o(vec![("usr/lib/gtk-3.0/3.0.0/immodules.cache".into(), so)]))
            }
            Emu::MimeDb => {
                let out = self.out_dir()?;
                self.mirror("usr/share/mime/packages", &out, "mime/packages", None)?;
                self.run(&out, "/usr/bin/update-mime-database", &[&format!("{VIEW_OUT}/mime")], &[0])?;
                let v = Self::files_below(&out.join("mime"), &["packages"])?.into_iter().map(|(r, p)| (format!("usr/share/mime/{r}"), p)).collect();
                ("update-mime-database".into(), o(v))
            }
            Emu::DesktopDb => {
                let out = self.out_dir()?;
                self.mirror("usr/share/applications", &out, "apps", Some(".desktop"))?;
                self.run(&out, "/usr/bin/update-desktop-database", &["-q", &format!("{VIEW_OUT}/apps")], &[0])?;
                ("update-desktop-database".into(), o(vec![("usr/share/applications/mimeinfo.cache".into(), Self::made(&out.join("apps/mimeinfo.cache"))?)]))
            }
            Emu::IconCache => {
                let mut v = Vec::new();
                for t in targets {
                    let d = t.trim_end_matches('/');
                    let Some(theme) = d.strip_prefix("usr/share/icons/") else { continue };
                    if !self.has(&format!("{d}/index.theme")) {
                        continue;
                    }
                    let out = self.out_dir()?;
                    self.mirror(d, &out, theme, None)?;
                    self.run(&out, "/usr/bin/gtk-update-icon-cache", &["-q", &format!("{VIEW_OUT}/{theme}")], &[0])?;
                    v.push((format!("{d}/icon-theme.cache"), Self::made(&out.join(theme).join("icon-theme.cache"))?));
                }
                ("gtk-update-icon-cache".into(), o(v))
            }
            Emu::FontconfigConfig => {
                // The hook's script (40-fontconfig-config): etc/fonts/conf.d/<name> -> /<target> unless present.
                let mut v = Vec::new();
                for t in targets {
                    let Some(base) = t.rsplit('/').next().filter(|b| !b.is_empty()) else { continue };
                    let link = format!("etc/fonts/conf.d/{base}");
                    if !self.has(&link) && self.has(t) {
                        v.push((link, format!("/{t}")));
                    }
                }
                ("ln".into(), Outcome::Links(v))
            }
            Emu::FcCacheFirstStart => (
                "fc-cache".into(),
                Outcome::Later("beim ersten Start: fontconfig legt seinen Cache selbst im Datenordner des Programms an (XDG_CACHE_HOME)".into()),
            ),
        })
    }

    /// Every hook of the closure that an install sets off, in hook order, with what happened.
    pub fn run_all(&mut self, closure: &[(String, &List)]) -> Vec<Result1> {
        let mut out = Vec::new();
        self.out_max = closure.iter().map(|(_, l)| super::pkgs::size_of(l)).fold(0u64, u64::saturating_add).max(OUT_MIN);
        for h in hooks(self.root, closure) {
            let h = match h {
                Ok(h) => h,
                Err(e) => {
                    event("pkg.trigger.unreadable", &[("reason", &e)]);
                    out.push(Result1 { hook: e.split(".hook").next().unwrap_or("?").to_string(), tool: String::new(), targets: Vec::new(), outcome: Outcome::NotRun(format!("unlesbar: {e}")) });
                    continue;
                }
            };
            let targets = alpmhook::targets(&h, closure);
            if targets.is_empty() || !h.post {
                if !targets.is_empty() {
                    out.push(Result1 { hook: h.name.clone(), tool: String::new(), targets, outcome: Outcome::NotRun(pre_reason(&h)) });
                }
                continue;
            }
            let res = if let Some(r) = refused(&h.name) {
                Result1 { hook: h.name.clone(), tool: String::new(), targets, outcome: Outcome::Refused(r.into()) }
            } else if let Some((_, emu)) = EMULATED.iter().find(|(n, _)| *n == h.name) {
                match tool(*emu) {
                    Some(t) if !self.has(t) => Result1 {
                        hook: h.name.clone(),
                        tool: String::new(),
                        targets,
                        outcome: Outcome::NotRun(format!("nicht ausgeführt: {t} fehlt im Baum ({})", if h.description.is_empty() { &h.exec } else { &h.description })),
                    },
                    _ => match self.emulate(*emu, &targets) {
                        Ok((tool, oc)) => Result1 { hook: h.name.clone(), tool, targets, outcome: oc },
                        Err(e) => Result1 { hook: h.name.clone(), tool: String::new(), targets, outcome: Outcome::Failed(e) },
                    },
                }
            } else {
                let what = if h.description.is_empty() { String::new() } else { format!("{}; ", h.description) };
                Result1 { hook: h.name.clone(), tool: String::new(), targets, outcome: Outcome::NotRun(format!("nicht ausgeführt (unbekannt: {what}Exec {})", h.exec)) }
            };
            let what = match &res.outcome {
                Outcome::Emulated(_) | Outcome::Links(_) => "emulated",
                Outcome::Later(_) => "later",
                Outcome::Refused(_) => "refused",
                Outcome::Failed(_) => "failed",
                Outcome::NotRun(_) => "not_run",
            };
            let reason = match &res.outcome {
                Outcome::Failed(r) | Outcome::Refused(r) | Outcome::NotRun(r) | Outcome::Later(r) => r.clone(),
                _ => String::new(),
            };
            event(&format!("pkg.trigger.{what}"), &[("hook", &res.hook), ("targets", &res.targets.len().to_string()), ("reason", &reason)]);
            out.push(res);
        }
        out
    }
}

/// A matching PreTransaction hook is never run (it would act on the host before the files exist). With
/// AbortOnFail pacman stops the whole transaction when it fails (hook.c:660): the report says so, since BitsOS
/// installs on regardless.
fn pre_reason(h: &alpmhook::Hook) -> String {
    let mut r = String::from("läuft vor der Installation (PreTransaction): nicht ausgeführt");
    if h.abort_on_fail {
        r.push_str("; mit AbortOnFail hätte pacman bei einem Fehler hier abgebrochen, BitsOS installiert weiter");
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_transaction_abort_on_fail_is_reported() {
        let h = alpmhook::parse("y", "[Trigger]\nType = Package\nOperation = Install\nTarget = glibc\n[Action]\nWhen = PreTransaction\nExec = /bin/false\nAbortOnFail\n").unwrap();
        assert!(pre_reason(&h).contains("hätte pacman bei einem Fehler hier abgebrochen"));
        let h = alpmhook::Hook { abort_on_fail: false, ..h };
        assert!(!pre_reason(&h).contains("abgebrochen") && pre_reason(&h).contains("nicht ausgeführt"));
    }

    #[test]
    fn tables_cover_the_plan() {
        for n in ["20-systemd-sysusers", "30-systemd-daemon-reload-system", "35-systemd-udev-reload", "60-depmod", "90-mkinitcpio-install", "40-update-ca-trust", "70-openssh-restart-sshd", "texinfo-install", "90-update-appstream-cache", "vimdoc"] {
            assert!(refused(n).is_some(), "{n}");
        }
        for n in ["11-glibc-ldconfig", "gio-querymodules", "update-desktop-database", "10-glibc-locale-gen"] {
            assert!(refused(n).is_none() && EMULATED.iter().any(|(e, _)| *e == n), "{n}");
        }
        assert!(refused("update-vlc-plugin-cache").is_none() && !EMULATED.iter().any(|(e, _)| *e == "update-vlc-plugin-cache"), "unknown: nicht ausgeführt");
        for n in ["fontconfig-32", "gio-querymodules-32", "12-lib32-glibc-iconvconfig"] {
            assert!(refused(n).is_some_and(|r| r.contains("32-Bit")), "{n}");
        }
        for n in ["fontconfig", "gio-querymodules", "12-glibc-iconvconfig"] {
            assert!(refused(n).is_none(), "{n}");
        }
    }

    /// A package's link on the way never leads a root-side read out of the tree.
    #[test]
    fn plain_refuses_links_on_the_way() {
        let d = std::env::temp_dir().join(format!("bitspkg-plain-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("usr/share/glib-2.0")).unwrap();
        fs::create_dir_all(d.join("usr/share/mime/packages")).unwrap();
        std::os::unix::fs::symlink("/etc", d.join("usr/share/glib-2.0/schemas")).unwrap();
        std::os::unix::fs::symlink("/usr/bin/true", d.join("usr/share/tool")).unwrap();
        assert!(plain(&d, "usr/share/glib-2.0/schemas", false).is_none());
        assert!(plain(&d, "usr/share/glib-2.0/schemas/passwd", false).is_none());
        assert!(plain(&d, "usr/share/tool", true).is_some(), "existence of a link is fine");
        assert!(plain(&d, "usr/share/mime/packages", false).is_some());
        assert!(plain(&d, "usr/share/../share", false).is_none());
        let work = d.join("work");
        fs::create_dir_all(&work).unwrap();
        let r = Runner::new(&d, &work);
        let out = d.join("out");
        assert!(r.mirror("usr/share/glib-2.0/schemas", &out, "s", None).is_err_and(|e| e.contains("nicht ausgeführt")));
        assert!(!out.join("s/passwd").exists());
        fs::remove_dir_all(&d).unwrap();
    }
}
