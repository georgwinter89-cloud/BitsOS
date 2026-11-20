//! The scripting set's commands (`python3`, `git`, `curl`, `jq`, `rg`; `skripte.txt`): `/bin/<command>` is a link
//! to bits, which starts the installed program's `/usr/bin/<command>` in a command view. That is bitsrun's program
//! view (the program's own tree as /usr and /etc, its own glibc; bitsrun's plan, unchanged) with the system's files
//! a command works on: the user's home, /tmp, /data and every other top folder of the system, the system's sockets
//! in /run/bitsos, the system's own programs (bits among them) at `SYSTEM_BIN`, the caller's folder and
//! environment. The Python module `bitsos` travels inside bits and is put on the interpreter's path from here.

use std::ffi::OsString;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use bitspkg::store::Store;
use bitsrun::facts::{self, FsTree};
use bitsrun::plan::{self, Input, Op};

use crate::out::{Done, Fail, R};

pub const SET: &str = include_str!("../skripte.txt");
/// Where the system's /bin appears in a command view (on PATH after the tree's /usr/bin).
pub const SYSTEM_BIN: &str = "/run/bitsos-system/bin";
/// The view's own folders; every other top folder of the system is bound as it is.
const OWN: &[&str] = &["usr", "bin", "sbin", "lib", "lib64", "etc", "dev", "proc", "sys", "tmp", "run"];

/// (program, commands) of the set, in its order.
pub fn members() -> Vec<(String, Vec<String>)> {
    SET.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            let mut w = l.split_whitespace().map(str::to_string);
            Some((w.next()?, w.collect()))
        })
        .collect()
}

/// The program a command of the set belongs to.
pub fn program_of(command: &str) -> Option<String> {
    members().into_iter().find(|(_, c)| c.iter().any(|x| x == command)).map(|(p, _)| p)
}

fn store_root() -> PathBuf {
    std::env::var_os("BITS_APPS").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

/// `bits befehl <command> [args]` or a link named after the command: never returns on success.
pub fn run(command: &str, args: Vec<OsString>) -> R {
    let prog = program_of(command).ok_or_else(|| Fail::usage(format!("„{command}“ gehört nicht zum Satz „skripte“ ({}).", members().iter().flat_map(|(_, c)| c.clone()).collect::<Vec<_>>().join(", "))))?;
    let uid = usys::getuid().map_err(|e| Fail::failed(format!("getuid: {e}")))? as u32;
    if usys::geteuid().map(|u| u == 0).unwrap_or(true) {
        return Err(Fail::refused(format!("{command} läuft wie jedes installierte Programm nicht als root: als Benutzer aufrufen.")));
    }
    let st = Store::new(&store_root());
    let v = st.current(&prog).ok_or_else(|| Fail::new("not_found", format!("{command} ist nicht installiert ({prog}); „bits programm satz skripte“ sagt, wie er dazukommt.")))?;
    let vd = st.version_dir(&prog, &v);
    let m = st.manifest(&prog, &v).map_err(Fail::failed)?;
    let tree_dir = if vd.join("root").is_dir() { vd.join("root") } else { vd };
    let tree_s = tree_dir.canonicalize().map_err(|e| Fail::failed(format!("{}: {e}", tree_dir.display())))?.to_string_lossy().into_owned();
    let program = plan::program_in_view(&format!("usr/bin/{command}"), false).map_err(Fail::failed)?;
    let data = data_folder(&st.data_dir(&prog), uid)?;
    let pw = facts::passwd(Path::new("/etc/passwd"), uid);
    let home = facts::home(std::env::var("HOME").ok().as_deref(), pw.as_ref(), uid);
    let user = pw.as_ref().map(|(n, _)| n.clone()).unwrap_or_else(|| format!("u{uid}"));
    let env: Vec<(String, String)> = std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))).collect();
    let tree = FsTree(PathBuf::from(&tree_s));
    let input = Input {
        tree: &tree,
        tree_dir: tree_s.clone(),
        name: Some(prog.clone()),
        data: Some(data),
        home: home.clone(),
        user,
        system: facts::system(Path::new("/etc"), uid, &home),
        permissions: &m.permissions,
        env: env.clone(),
        program,
        args,
    };
    let mut p = plan::build(&input).map_err(Fail::failed)?;
    let module = crate::py::install().map_err(Fail::failed)?;
    let tops = top_folders();
    let has_home = Path::new(&home).is_dir();
    widen(&mut p, &home, has_home, &tops, Path::new("/run/bitsos").is_dir());
    p.env = command_env(&env, &p.env, &module);
    let cwd = std::env::current_dir().ok().map(|d| d.to_string_lossy().into_owned());
    // A machine without the user's home yet (a fresh /data): the command starts in /tmp.
    p.chdir = cwd.filter(|d| shown(d, &home, &tops)).unwrap_or_else(|| if has_home { home } else { "/tmp".into() });
    let bwrap = std::env::var("BITS_BWRAP").unwrap_or_else(|_| bitsrun::bwrap::BWRAP.to_string());
    let e = std::process::Command::new(&bwrap).args(bitsrun::bwrap::args(&p)).env_clear().envs(p.env.iter().cloned()).exec();
    Err(Fail::failed(format!("{bwrap} lässt sich nicht starten: {e}")))
}

/// The data folder as bitsrun keeps it: made 0700 when missing, and it must be the caller's.
fn data_folder(d: &Path, uid: u32) -> R<String> {
    if !d.exists() {
        bitspkg::fsx::mkdir(d, 0o700).map_err(Fail::failed)?;
    }
    let m = std::fs::symlink_metadata(d).map_err(|e| Fail::failed(format!("{}: {e}", d.display())))?;
    if !m.is_dir() || m.uid() != uid {
        return Err(Fail::failed(format!("{}: der Datenordner ist kein Ordner dieses Benutzers", d.display())));
    }
    Ok(d.canonicalize().map_err(|e| Fail::failed(e.to_string()))?.to_string_lossy().into_owned())
}

/// Whether the caller's folder is the same folder in the view (else the command starts in the home).
pub fn shown(dir: &str, home: &str, tops: &[String]) -> bool {
    let under = |base: &str| dir == base || dir.starts_with(&format!("{base}/"));
    under("/tmp") || under(home) || tops.iter().any(|t| under(t))
}

/// The system's top folders a command view binds as they are (real folders, not links).
fn top_folders() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir("/")
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                // A folder the caller cannot read (root's) cannot be bound by it either.
                .filter(|e| std::fs::read_dir(e.path()).is_ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| !OWN.contains(&n.as_str()))
                .map(|n| format!("/{n}"))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// bitsrun's program view turned into the command view (pure, host-tested): the shared /tmp and home instead of
/// private ones, nothing placed inside them, the system's folders, its sockets and its programs.
pub fn widen(p: &mut plan::Plan, home: &str, has_home: bool, tops: &[String], run_bitsos: bool) {
    // Nothing is placed inside the home or a bound system folder: bwrap would make its mount point in the real one
    // (the data folder's second place, the fonts; a store's data folder elsewhere than /data/appdata).
    let inside_home = |d: &str| d.starts_with(&format!("{home}/")) || tops.iter().any(|t| d.starts_with(&format!("{t}/")));
    let mut ops = Vec::new();
    for op in std::mem::take(&mut p.ops) {
        match op {
            Op::Tmpfs { dst, .. } if dst == "/tmp" => ops.push(Op::Bind { src: "/tmp".into(), dst }),
            Op::Dir { dst, .. } if dst == home => {
                for t in tops {
                    ops.push(Op::Bind { src: t.clone(), dst: t.clone() });
                }
                if has_home && !tops.iter().any(|t| home.starts_with(&format!("{t}/"))) {
                    ops.push(Op::Bind { src: home.to_string(), dst: home.to_string() });
                }
            }
            Op::RoBind { dst, .. } | Op::Bind { dst, .. } if inside_home(&dst) => {}
            Op::RemountRo { dst } => {
                if run_bitsos {
                    ops.push(Op::Bind { src: "/run/bitsos".into(), dst: "/run/bitsos".into() });
                }
                ops.push(Op::RoBind { src: "/bin".into(), dst: SYSTEM_BIN.into() });
                ops.push(Op::RemountRo { dst });
            }
            o => ops.push(o),
        }
    }
    p.ops = ops;
}

/// The caller's environment for a command (its own variables pass, unlike a program's view), without what would
/// load foreign libraries, with the view's PATH and runtime folder and the module on Python's path.
pub fn command_env(caller: &[(String, String)], view: &[(String, String)], module: &Path) -> Vec<(String, String)> {
    let drop = |k: &str| k.starts_with("LD_") || matches!(k, "PATH" | "PYTHONHOME" | "PYTHONPATH" | "XDG_CONFIG_HOME" | "XDG_CACHE_HOME");
    let mut env: Vec<(String, String)> = caller.iter().filter(|(k, _)| !drop(k)).cloned().collect();
    for (k, v) in view {
        if !drop(k) && !env.iter().any(|(e, _)| e == k) {
            env.push((k.clone(), v.clone()));
        }
    }
    env.retain(|(k, _)| k != "XDG_RUNTIME_DIR");
    if let Some((_, r)) = view.iter().find(|(k, _)| k == "XDG_RUNTIME_DIR") {
        env.push(("XDG_RUNTIME_DIR".into(), r.clone()));
    }
    env.push(("PATH".into(), format!("/usr/bin:{SYSTEM_BIN}")));
    let mut pp = module.to_string_lossy().into_owned();
    if let Some((_, old)) = caller.iter().find(|(k, _)| k == "PYTHONPATH") {
        pp = format!("{pp}:{old}");
    }
    env.push(("PYTHONPATH".into(), pp));
    env.push(("BITS".into(), format!("{SYSTEM_BIN}/bits")));
    // The view binds the system's CA bundle at this name, but OpenSSL looks for a cert.pem or hashed names there: a
    // program whose closure has no ca-certificates (python) would trust nothing.
    if !caller.iter().any(|(k, _)| k == "SSL_CERT_FILE") && view_has_ca() {
        env.push(("SSL_CERT_FILE".into(), CA_BUNDLE.into()));
    }
    env
}

pub const CA_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";

fn view_has_ca() -> bool {
    Path::new(CA_BUNDLE).is_file()
}

/// `bits programm befehle`: the set's commands and whether their program is installed.
pub fn list() -> R {
    let st = Store::new(&store_root());
    let rows: Vec<serde_json::Value> = members()
        .into_iter()
        .map(|(p, c)| serde_json::json!({"program": p, "commands": c, "installed": st.current(&p)}))
        .collect();
    let text = rows.iter().map(|r| format!("{:<10} {:<20} {}", r["program"].as_str().unwrap_or(""), r["commands"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" ")).unwrap_or_default(), r["installed"].as_str().unwrap_or("nicht installiert"))).collect::<Vec<_>>().join("\n");
    Ok(Done::new(text).with("commands", rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_set_names_each_command_once() {
        let m = members();
        assert_eq!(m.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), ["python", "git", "curl", "jq", "ripgrep"]);
        assert_eq!(program_of("python3").as_deref(), Some("python"));
        assert_eq!(program_of("rg").as_deref(), Some("ripgrep"));
        assert_eq!(program_of("bash"), None);
        let all: Vec<String> = m.iter().flat_map(|(_, c)| c.clone()).collect();
        let mut uniq = all.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(all.len(), uniq.len());
    }

    #[test]
    fn the_command_view_shares_tmp_home_and_the_system() {
        let mut p = plan::Plan {
            ops: vec![
                Op::Tmpfs { dst: "/tmp".into(), mode: 0o1777 },
                Op::Dir { dst: "/run".into(), mode: 0o755 },
                Op::Dir { dst: "/data/home/1000".into(), mode: 0o700 },
                Op::RoBind { src: "/usr/share/fonts".into(), dst: "/data/home/1000/.local/share/fonts".into() },
                Op::Bind { src: "/data/appdata/python".into(), dst: "/data/appdata/python".into() },
                Op::Bind { src: "/data/appdata/python".into(), dst: "/data/home/1000/.local/share/python".into() },
                Op::RemountRo { dst: "/".into() },
            ],
            env: vec![],
            chdir: "/data/home/1000".into(),
            argv: vec![],
            notes: vec![],
        };
        widen(&mut p, "/data/home/1000", true, &["/apps".into(), "/data".into()], true);
        let shown: Vec<String> = p.ops.iter().map(|o| o.to_string()).collect();
        assert_eq!(
            shown,
            [
                "bind /tmp -> /tmp",
                "dir /run 0755",
                "bind /apps -> /apps",
                "bind /data -> /data",
                "bind /run/bitsos -> /run/bitsos",
                "ro-bind /bin -> /run/bitsos-system/bin",
                "remount-ro /",
            ]
        );
    }

    #[test]
    fn a_missing_home_is_not_bound() {
        let mut p = plan::Plan { ops: vec![Op::Dir { dst: "/home/u".into(), mode: 0o700 }, Op::RemountRo { dst: "/".into() }], env: vec![], chdir: String::new(), argv: vec![], notes: vec![] };
        widen(&mut p, "/home/u", false, &[], false);
        assert_eq!(p.ops.iter().map(|o| o.to_string()).collect::<Vec<_>>(), ["ro-bind /bin -> /run/bitsos-system/bin", "remount-ro /"]);
    }

    #[test]
    fn the_callers_folder_is_kept_where_the_view_shows_it() {
        let tops = ["/data".to_string(), "/apps".to_string()];
        assert!(shown("/data/home/1000/projekt", "/data/home/1000", &tops));
        assert!(shown("/tmp", "/data/home/1000", &tops));
        assert!(!shown("/usr/share", "/data/home/1000", &tops), "the view's /usr is the program's");
        assert!(!shown("/database", "/data/home/1000", &tops));
    }

    #[test]
    fn the_callers_variables_pass_but_no_loader_tricks() {
        let caller = vec![("LD_PRELOAD".into(), "x.so".into()), ("FOO".into(), "1".into()), ("PATH".into(), "/bin".into()), ("PYTHONPATH".into(), "/mine".into())];
        let view = vec![("XDG_RUNTIME_DIR".into(), "/run/user/1000".into()), ("XDG_CACHE_HOME".into(), "/h/.local/share/p/cache".into())];
        let env = command_env(&caller, &view, Path::new("/run/user/1000/bits-python/1"));
        let get = |k: &str| env.iter().find(|(e, _)| e == k).map(|(_, v)| v.as_str());
        assert_eq!(get("LD_PRELOAD"), None);
        assert_eq!(get("FOO"), Some("1"));
        assert_eq!(get("XDG_CACHE_HOME"), None);
        assert_eq!(get("PATH"), Some("/usr/bin:/run/bitsos-system/bin"));
        assert_eq!(get("PYTHONPATH"), Some("/run/user/1000/bits-python/1:/mine"));
        assert_eq!(get("XDG_RUNTIME_DIR"), Some("/run/user/1000"));
    }
}
