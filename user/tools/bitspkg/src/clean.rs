//! PK-2's "no leftovers" round trip, the same on CachyOS (tests/clean.rs) and on V2 (`bitspkg selftest`, ktest
//! ul.tools.cases): a private settingsd (settingsd's own library on a thread, files in a scratch folder), a test
//! package with a settings key, a wanted permission and an action; its program writes into its data folder and
//! tries to write elsewhere; a settings value is set, settingsd "reboots" (forgets every registration); then
//! the package is removed. The whole scratch tree, settingsd's keys and the rule/action ledgers must be what they
//! were before the install.
//!
//! What refuses the stray write today is Unix permissions only (the program runs as uid 1000, the folders are
//! root's): B3 refusals come with bitsrun's views (PK-3, FS-11) and permd (PM-3). Whatever the program does get
//! to write outside its data folder is a leftover this check reports.

use crate::hooks::{Hook, Hooks};
use crate::manifest::Manifest;
use crate::settings::Settings;
use crate::store::Store;
use crate::{build, fsx, install, remove, snapshot};
use settingsd::json::Json;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SPEC: &str = r##"format = 1
name = "clean"
version = "1"
program = "bin/run"

[[permission]]
kind = "net_connect"
proto = "tcp"
port = 443
why = "PK-2 selftest"

[[action]]
name = "ping"
safety = "read"
de = "Antwortet"
en = "Answers"

[[setting]]
key = "app.clean.color"
type = "color"
default = "#000000"
de = "Farbe"
en = "Colour"
"##;

/// Writes its profile into its data folder ($1) and tries to write elsewhere ($2) and into its own package ($3).
const PROGRAM: &str = r#"#!/bin/sh
echo profile > "$1/profile" || exit 3
mkdir -p "$1/cache" && echo c > "$1/cache/x" || exit 4
for d in "$2" "$3"; do
  if echo stray 2>/dev/null > "$d/stray"; then echo "wrote $d/stray"; else echo "refused $d/stray"; fi
done
"#;

/// The uid the program runs as when bitspkg runs as root (Brave's, BR-2).
const PROGRAM_UID: u32 = 1000;

/// A stand-in for permd (rules) or MC-7 (actions): what install registered, per package.
pub struct Ledger {
    pub what: &'static str,
    pub table: Arc<Mutex<BTreeMap<String, Vec<String>>>>,
    pub of: fn(&Manifest) -> Vec<String>,
}

impl Hook for Ledger {
    fn what(&self) -> &'static str {
        self.what
    }
    fn needed(&self, ms: &[Manifest]) -> bool {
        ms.iter().any(|m| !(self.of)(m).is_empty())
    }
    fn install(&self, m: &Manifest) -> Result<(), String> {
        let v = (self.of)(m);
        let mut t = self.table.lock().map_err(|_| "ledger poisoned")?;
        if v.is_empty() {
            t.remove(&m.name);
        } else {
            t.insert(m.name.clone(), v);
        }
        Ok(())
    }
    fn remove(&self, name: &str, _: &[Manifest], _: bool) -> Result<(), String> {
        self.table.lock().map_err(|_| "ledger poisoned")?.remove(name);
        Ok(())
    }
}

/// settingsd's server on a thread of this process, its files in `dir`, its socket `sock`.
pub fn start_settingsd(dir: &Path, sock: &Path) -> Result<(), String> {
    use settingsd::{events::Events, registry::Registry, server, store::Paths};
    for d in ["stick", "fallback", "sys"] {
        fs::create_dir_all(dir.join(d)).map_err(|e| fsx::err(&dir.join(d), e))?;
    }
    let paths = Paths { stick: dir.join("stick"), fallback: dir.join("fallback"), sys: dir.join("sys"), socket: sock.to_path_buf() };
    let reg = Registry::open(paths, Events::capture().0, Duration::from_secs(5));
    let l = server::bind(sock, &reg)?;
    let shared = Arc::new(server::Shared::new(reg));
    std::thread::Builder::new().name("settingsd".into()).spawn(move || server::serve(l, shared)).map_err(|e| e.to_string())?;
    Ok(())
}

fn ask(s: &Settings, req: Json) -> Result<Json, String> {
    s.request(&req)?.map_err(|(c, m)| format!("settingsd: {c}: {m}"))
}

fn keys(s: &Settings) -> Result<String, String> {
    Ok(ask(s, Json::obj().with("op", "list").with("prefix", "app"))?.get("keys").map(Json::to_text).unwrap_or_default())
}

fn put(p: &Path, data: &str, mode: u32) -> Result<(), String> {
    fs::create_dir_all(p.parent().unwrap_or(p)).map_err(|e| fsx::err(p, e))?;
    fs::write(p, data).map_err(|e| fsx::err(p, e))?;
    fs::set_permissions(p, fs::Permissions::from_mode(mode)).map_err(|e| fsx::err(p, e))
}

/// The round trip in `work` (made empty first). Ok = the lines it checked.
pub fn round_trip(work: &Path) -> Result<Vec<String>, String> {
    let mut ok = Vec::new();
    let mut need = |c: bool, what: String| if c { ok.push(what); Ok(()) } else { Err(format!("no leftovers: {what}: failed")) };
    fsx::remove_tree(work)?;
    let (top, run) = (work.join("tree"), work.join("run"));
    let pkg = work.join("pkg");
    put(&work.join("src/package.toml"), SPEC, 0o644)?;
    put(&work.join("src/bin/run"), PROGRAM, 0o755)?;
    build::build(&work.join("src"), None, Some(&pkg))?;
    let elsewhere = top.join("elsewhere");
    fs::create_dir_all(&elsewhere).map_err(|e| fsx::err(&elsewhere, e))?;
    let root = is_root();
    // Root's folder is closed to uid 1000 by its 0755; a normal user's own folder needs 0555.
    fs::set_permissions(&elsewhere, fs::Permissions::from_mode(if root { 0o755 } else { 0o555 })).map_err(|e| fsx::err(&elsewhere, e))?;
    start_settingsd(&top.join("settings"), &run.join("boot1.sock"))?;
    let rules = Arc::new(Mutex::new(BTreeMap::new()));
    let actions = Arc::new(Mutex::new(BTreeMap::new()));
    let hooks = |sock: &str| {
        let mut h = Hooks::standard(Some(run.join(sock)));
        h.list.truncate(1);
        h.list.push(Box::new(Ledger { what: "rules", table: rules.clone(), of: |m| m.permissions.iter().map(|p| p.describe()).collect() }));
        h.list.push(Box::new(Ledger { what: "actions", table: actions.clone(), of: |m| m.actions.iter().map(|a| format!("app.{}.{}", m.name, a.name)).collect() }));
        h
    };
    let mut st = Store::new(&top.join("root"));
    st.hooks = hooks("boot1.sock");
    let s1 = Settings { socket: run.join("boot1.sock") };
    // A system with a history: a value and a journal entry of its own (with an empty journal settingsd leaves an
    // empty journal.log behind after a forget: change request to CL, plan/notes/pk-PK-2.md).
    ask(&s1, Json::obj().with("op", "set").with("key", "theme.accent").with("value", "#123456").with("source", "bitspkg-selftest"))?;
    // The store exists before (its lock, the ProgramId counter after a first package): only `clean` is new.
    crate::verify::list(&st)?;
    st.lock()?;
    let before = snapshot::take(&top)?;
    let keys_before = keys(&s1)?;

    let r = install::install(&st, &pkg)?;
    need(r.hooks_failed == 0 && keys(&s1)?.contains("app.clean.color"), "install registers app.clean.color".into())?;
    need(rules.lock().map(|t| t.contains_key("clean")).unwrap_or(false), "install hands the wanted permission to the rules hook".into())?;
    need(actions.lock().map(|t| t.get("clean").is_some_and(|v| v == &["app.clean.ping"])).unwrap_or(false), "install registers app.clean.ping".into())?;
    let set = Json::obj().with("op", "set").with("key", "app.clean.color").with("value", "#123456").with("source", "bitspkg-selftest");
    ask(&s1, set)?;
    let data = st.data_dir("clean");
    let cur = st.pkg("clean").join("current");
    let mut cmd = std::process::Command::new(cur.join("bin/run"));
    cmd.args([&data, &elsewhere, &cur]);
    if root {
        std::os::unix::fs::chown(&data, Some(PROGRAM_UID), Some(PROGRAM_UID)).map_err(|e| fsx::err(&data, e))?;
        std::os::unix::process::CommandExt::uid(&mut cmd, PROGRAM_UID);
        std::os::unix::process::CommandExt::gid(&mut cmd, PROGRAM_UID);
    }
    let o = cmd.output().map_err(|e| format!("running the test program: {e}"))?;
    let out = String::from_utf8_lossy(&o.stdout).to_string();
    let who = if root { format!("as uid {PROGRAM_UID}") } else { "as this user".into() };
    need(o.status.success() && data.join("cache/x").is_file(), format!("the program ({who}) writes into its data folder"))?;
    need(!out.contains("wrote"), format!("its writes elsewhere are refused (Unix permissions): {}", out.trim().replace('\n', "; ")))?;

    // A reboot: the new settingsd knows no package key, the value waits as an orphan in its file.
    start_settingsd(&top.join("settings"), &run.join("boot2.sock"))?;
    st.hooks = hooks("boot2.sock");
    let s2 = Settings { socket: run.join("boot2.sock") };
    need(!keys(&s2)?.contains("app.clean.color"), "after a settingsd restart the key is not registered".into())?;
    remove::remove(&st, "clean", false)?;
    let d = snapshot::diff(&before, &snapshot::take(&top)?, &["root/apps/.next-id"]);
    need(d.is_empty(), format!("after the removal the tree is the tree before the install{}", if d.is_empty() { String::new() } else { format!(":\n  {}", d.join("\n  ")) }))?;
    need(keys(&s2)? == keys_before, "settingsd's keys are those before the install".into())?;
    let empty = |t: &Arc<Mutex<BTreeMap<String, Vec<String>>>>| t.lock().map(|t| t.is_empty()).unwrap_or(false);
    need(empty(&rules) && empty(&actions), "no rules and no actions left".into())?;
    fsx::remove_tree(work)?;
    Ok(ok)
}

fn is_root() -> bool {
    usys::geteuid() == Ok(0)
}

/// Where tests put the work folder: `$dir/bitspkg-clean`.
pub fn work_in(dir: &Path) -> PathBuf {
    dir.join("bitspkg-clean")
}
