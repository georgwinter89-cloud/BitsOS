//! `manifest.toml` (plan/PACKAGES.md): what a package is, what it brings and what it wants. `from_text` checks
//! everything a hostile package could get wrong; a `Manifest` that exists is safe to install.

use crate::names::{check_abs_path, check_ident, check_link_target, check_name, check_rel_path, check_sha256, check_version};
use crate::perm::Permission;
use crate::toml::{self, Doc, Table, Value};
use settingsd::json::Json;
use settingsd::key::KeyDef;
use std::collections::HashSet;

pub const FORMAT: i64 = 1;
pub const FILE_NAME: &str = "manifest.toml";
/// MCP tool names stay short enough for every client (the MCP spec allows 1..128).
const TOOL_NAME_MAX: usize = 128;

#[derive(Clone, Debug, PartialEq)]
pub struct FileEntry {
    pub path: String,
    pub sha256: String,
    pub size: u64,
    pub exec: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Symlink {
    pub path: String,
    pub target: String,
}

/// B5: an action Claude may call as the MCP tool `app.<package>.<name>` (MC-7 reads these).
#[derive(Clone, Debug, PartialEq)]
pub struct Action {
    pub name: String,
    pub safety: String,
    pub de: String,
    pub en: String,
    /// JSON Schema texts (checked to be JSON objects).
    pub args: String,
    pub result: String,
}

/// A program the service manager (`svc`, A6) keeps running.
#[derive(Clone, Debug, PartialEq)]
pub struct Service {
    pub name: String,
    pub exec: String,
    pub args: Vec<String>,
    pub class: String,
    pub restart: String,
}

/// A settings key under `app.<package>.`; `def` is the definition PK-2 registers with settingsd, `table` the
/// manifest text it came from (written back unchanged).
#[derive(Clone, Debug, PartialEq)]
pub struct Setting {
    pub def: KeyDef,
    pub table: Table,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub de: String,
    pub en: String,
    /// The main program inside the package, for `bitsrun` (PK-3).
    pub program: Option<String>,
    pub files: Vec<FileEntry>,
    pub symlinks: Vec<Symlink>,
    pub dirs: Vec<String>,
    pub permissions: Vec<Permission>,
    pub actions: Vec<Action>,
    pub services: Vec<Service>,
    pub settings: Vec<Setting>,
}

pub(crate) struct Fields<'a> {
    t: &'a Table,
    what: String,
    used: Vec<&'static str>,
}

impl<'a> Fields<'a> {
    pub(crate) fn new(t: &'a Table, what: String) -> Self {
        Fields { t, what, used: Vec::new() }
    }
    fn err(&self, e: impl std::fmt::Display) -> String {
        format!("{} (line {}): {e}", self.what, self.t.line)
    }
    pub(crate) fn opt_str(&mut self, k: &'static str) -> Result<Option<String>, String> {
        self.used.push(k);
        match self.t.get(k) {
            None => Ok(None),
            Some(Value::Str(s)) => Ok(Some(s.clone())),
            Some(_) => Err(self.err(format!("{k} must be a string"))),
        }
    }
    pub(crate) fn str(&mut self, k: &'static str) -> Result<String, String> {
        self.opt_str(k)?.ok_or_else(|| self.err(format!("{k} missing")))
    }
    pub(crate) fn opt_int(&mut self, k: &'static str) -> Result<Option<i64>, String> {
        self.used.push(k);
        match self.t.get(k) {
            None => Ok(None),
            Some(Value::Int(n)) => Ok(Some(*n)),
            Some(_) => Err(self.err(format!("{k} must be an integer"))),
        }
    }
    pub(crate) fn bool(&mut self, k: &'static str) -> Result<bool, String> {
        self.used.push(k);
        match self.t.get(k) {
            None => Ok(false),
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => Err(self.err(format!("{k} must be true or false"))),
        }
    }
    fn strs(&mut self, k: &'static str) -> Result<Vec<String>, String> {
        self.used.push(k);
        match self.t.get(k) {
            None => Ok(Vec::new()),
            Some(Value::Arr(a)) => a
                .iter()
                .map(|v| match v {
                    Value::Str(s) => Ok(s.clone()),
                    _ => Err(self.err(format!("{k} must be an array of strings"))),
                })
                .collect(),
            Some(_) => Err(self.err(format!("{k} must be an array of strings"))),
        }
    }
    pub(crate) fn wrap<T>(&self, r: Result<T, String>) -> Result<T, String> {
        r.map_err(|e| self.err(e))
    }
    /// Unknown keys are refused: a typo must not silently drop a permission or a check.
    pub(crate) fn done(self) -> Result<(), String> {
        match self.t.entries.iter().find(|(k, _)| !self.used.contains(&k.as_str())) {
            Some((k, _)) => Err(self.err(format!("unknown key {k}"))),
            None => Ok(()),
        }
    }
}

fn one_of(f: &Fields, k: &str, v: String, allowed: &[&str]) -> Result<String, String> {
    if allowed.contains(&v.as_str()) { Ok(v) } else { Err(f.err(format!("{k} {v:?}: want one of {}", allowed.join("|")))) }
}

fn json_object(f: &Fields, k: &str, text: Option<String>) -> Result<String, String> {
    let t = text.unwrap_or_else(|| "{\"type\":\"object\"}".into());
    match settingsd::json::parse(&t) {
        Ok(Json::Obj(_)) => Ok(t),
        Ok(_) => Err(f.err(format!("{k} must be a JSON object (a JSON Schema)"))),
        Err(e) => Err(f.err(format!("{k}: {e}"))),
    }
}

impl Manifest {
    pub fn from_text(text: &str) -> Result<Manifest, String> {
        Self::from_doc(&toml::parse(text)?)
    }

    pub fn from_doc(doc: &Doc) -> Result<Manifest, String> {
        let mut f = Fields::new(&doc.root, "manifest".into());
        match f.opt_int("format")? {
            Some(FORMAT) => {}
            Some(n) => return Err(f.err(format!("format {n} is newer than this bitspkg ({FORMAT})"))),
            None => return Err(f.err("format missing")),
        }
        let name = f.str("name")?;
        f.wrap(check_name(&name))?;
        let version = f.str("version")?;
        f.wrap(check_version(&version))?;
        let de = f.opt_str("de")?.unwrap_or_default();
        let en = f.opt_str("en")?.unwrap_or_default();
        let program = f.opt_str("program")?;
        f.done()?;
        let mut m = Manifest {
            name,
            version,
            de,
            en,
            program,
            files: Vec::new(),
            symlinks: Vec::new(),
            dirs: Vec::new(),
            permissions: Vec::new(),
            actions: Vec::new(),
            services: Vec::new(),
            settings: Vec::new(),
        };
        for (kind, t) in &doc.arrays {
            m.entry(kind, t)?;
        }
        m.check_tree()?;
        m.check_refs()?;
        Ok(m)
    }

    fn entry(&mut self, kind: &str, t: &Table) -> Result<(), String> {
        let mut f = Fields::new(t, format!("[[{kind}]]"));
        match kind {
            "file" => {
                let path = f.str("path")?;
                f.wrap(check_rel_path(&path))?;
                let sha256 = f.str("sha256")?;
                f.wrap(check_sha256(&sha256))?;
                let size = f.opt_int("size")?.ok_or_else(|| f.err("size missing"))?;
                let size = u64::try_from(size).map_err(|_| f.err("size is negative"))?;
                let exec = f.bool("exec")?;
                self.files.push(FileEntry { path, sha256, size, exec });
            }
            "symlink" => {
                let path = f.str("path")?;
                f.wrap(check_rel_path(&path))?;
                let target = f.str("target")?;
                f.wrap(check_link_target(&path, &target))?;
                self.symlinks.push(Symlink { path, target });
            }
            "dir" => {
                let path = f.str("path")?;
                f.wrap(check_rel_path(&path))?;
                self.dirs.push(path);
            }
            "permission" => self.permissions.push(Permission::from_fields(&mut f)?),
            "action" => {
                let name = f.str("name")?;
                f.wrap(check_ident("action", &name))?;
                let tool = format!("app.{}.{name}", self.name);
                if tool.len() > TOOL_NAME_MAX {
                    return Err(f.err(format!("tool name {tool} longer than {TOOL_NAME_MAX}")));
                }
                let safety = f.str("safety")?;
                let safety = one_of(&f, "safety", safety, &["read", "change", "delete_overwrite"])?;
                let de = f.str("de")?;
                let en = f.str("en")?;
                let a = f.opt_str("args")?;
                let args = json_object(&f, "args", a)?;
                let r = f.opt_str("result")?;
                let result = json_object(&f, "result", r)?;
                self.actions.push(Action { name, safety, de, en, args, result });
            }
            "service" => {
                let name = f.str("name")?;
                f.wrap(check_ident("service", &name))?;
                let exec = f.str("exec")?;
                f.wrap(check_rel_path(&exec))?;
                let args = f.strs("args")?;
                let class = f.opt_str("class")?.unwrap_or_else(|| "normal".into());
                let class = one_of(&f, "class", class, &["normal", "background"])?;
                let restart = f.opt_str("restart")?.unwrap_or_else(|| "on-failure".into());
                let restart = one_of(&f, "restart", restart, &["always", "on-failure", "never"])?;
                self.services.push(Service { name, exec, args, class, restart });
            }
            "setting" => {
                let def = setting_def(&self.name, &mut f)?;
                self.settings.push(Setting { def, table: t.clone() });
            }
            _ => return Err(f.err("unknown entry kind")),
        }
        f.done()
    }

    /// Every path once (also ASCII case-blind: the package is copied over FAT), `manifest.toml` reserved at
    /// the top, and no file or link used as a folder of another entry (so no install ever writes through a link).
    fn check_tree(&self) -> Result<(), String> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut leaves: HashSet<String> = HashSet::new();
        let leaf_paths = self.files.iter().map(|e| &e.path).chain(self.symlinks.iter().map(|s| &s.path));
        for p in leaf_paths.clone() {
            leaves.insert(p.to_ascii_lowercase());
        }
        for p in leaf_paths.chain(self.dirs.iter()) {
            let low = p.to_ascii_lowercase();
            if low == FILE_NAME {
                return Err(format!("{p}: {FILE_NAME} at the top is the manifest's own name"));
            }
            if !seen.insert(low.clone()) {
                return Err(format!("{p} is listed twice (names are compared case-blind)"));
            }
            let mut at = 0;
            while let Some(i) = low[at..].find('/') {
                let prefix = &low[..at + i];
                if leaves.contains(prefix) {
                    return Err(format!("{p}: {prefix} is a file or symlink, not a folder"));
                }
                at += i + 1;
            }
        }
        Ok(())
    }

    fn check_refs(&self) -> Result<(), String> {
        let runnable = |p: &str| {
            self.files.iter().any(|e| e.path == p && e.exec) || self.symlinks.iter().any(|s| s.path == p)
        };
        if let Some(p) = &self.program {
            check_rel_path(p)?;
            if !runnable(p) {
                return Err(format!("program {p} is not an executable file or symlink of the package"));
            }
        }
        let mut names = HashSet::new();
        for s in &self.services {
            if !runnable(&s.exec) {
                return Err(format!("service {}: {} is not an executable file or symlink of the package", s.name, s.exec));
            }
            if !names.insert(&s.name) {
                return Err(format!("service {} listed twice", s.name));
            }
        }
        let mut names = HashSet::new();
        for a in &self.actions {
            if !names.insert(&a.name) {
                return Err(format!("action {} listed twice", a.name));
            }
        }
        let mut names = HashSet::new();
        for s in &self.settings {
            if !names.insert(&s.def.name) {
                return Err(format!("setting {} listed twice", s.def.name));
            }
        }
        Ok(())
    }

    pub fn to_doc(&self) -> Doc {
        let s = |v: &str| Value::Str(v.to_string());
        let mut root = Table::default();
        let put = |t: &mut Table, k: &str, v: Value| t.entries.push((k.to_string(), v));
        put(&mut root, "format", Value::Int(FORMAT));
        put(&mut root, "name", s(&self.name));
        put(&mut root, "version", s(&self.version));
        for (k, v) in [("de", &self.de), ("en", &self.en)] {
            if !v.is_empty() {
                put(&mut root, k, s(v));
            }
        }
        if let Some(p) = &self.program {
            put(&mut root, "program", s(p));
        }
        let mut arrays = Vec::new();
        let mut add = |kind: &str, entries: Vec<(&str, Value)>| {
            let t = Table { line: 0, entries: entries.into_iter().map(|(k, v)| (k.to_string(), v)).collect() };
            arrays.push((kind.to_string(), t));
        };
        for e in &self.files {
            let mut v = vec![("path", s(&e.path)), ("sha256", s(&e.sha256)), ("size", Value::Int(e.size as i64))];
            if e.exec {
                v.push(("exec", Value::Bool(true)));
            }
            add("file", v);
        }
        for l in &self.symlinks {
            add("symlink", vec![("path", s(&l.path)), ("target", s(&l.target))]);
        }
        for d in &self.dirs {
            add("dir", vec![("path", s(d))]);
        }
        for p in &self.permissions {
            add("permission", p.to_entries());
        }
        for a in &self.actions {
            add(
                "action",
                vec![
                    ("name", s(&a.name)),
                    ("safety", s(&a.safety)),
                    ("de", s(&a.de)),
                    ("en", s(&a.en)),
                    ("args", s(&a.args)),
                    ("result", s(&a.result)),
                ],
            );
        }
        for v in &self.services {
            let args = Value::Arr(v.args.iter().map(|a| s(a)).collect());
            add(
                "service",
                vec![("name", s(&v.name)), ("exec", s(&v.exec)), ("args", args), ("class", s(&v.class)), ("restart", s(&v.restart))],
            );
        }
        for st in &self.settings {
            add("setting", st.table.entries.iter().map(|(k, v)| (k.as_str(), v.clone())).collect());
        }
        Doc { root, arrays }
    }

    pub fn to_text(&self) -> String {
        toml::write(&self.to_doc())
    }

    /// Bytes of all files (each counted once per package, shared content is counted in every package).
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|e| e.size).fold(0u64, u64::saturating_add)
    }
}

/// A `[[setting]]` as settingsd's registration (KeyDef::from_json) reads it, so the manifest cannot carry a key
/// the registry would refuse (R8 switches, kernel secrets, a default outside its type).
fn setting_def(pkg: &str, f: &mut Fields) -> Result<KeyDef, String> {
    let key = f.str("key")?;
    let own = format!("app.{pkg}.");
    if !key.starts_with(&own) {
        return Err(f.err(format!("setting {key}: keys of this package start with {own}")));
    }
    let ty = f.str("type")?;
    let mut schema = vec![("type".to_string(), Json::Str(ty))];
    for k in ["min", "max", "max_bytes"] {
        if let Some(n) = f.opt_int(k)? {
            schema.push((k.to_string(), Json::Int(n)));
        }
    }
    if let Some(u) = f.opt_str("unit")? {
        schema.push(("unit".into(), Json::Str(u)));
    }
    let choices = f.strs("choices")?;
    if !choices.is_empty() {
        schema.push(("choices".into(), Json::Arr(choices.into_iter().map(Json::Str).collect())));
    }
    f.used.push("default");
    let default = match f.t.get("default") {
        Some(Value::Str(s)) => Json::Str(s.clone()),
        Some(Value::Int(n)) => Json::Int(*n),
        Some(Value::Bool(b)) => Json::Bool(*b),
        Some(Value::Arr(_)) => return Err(f.err("default must be a string, integer or boolean")),
        None => return Err(f.err("default missing")),
    };
    let text = Json::Obj(vec![("de".into(), Json::Str(f.str("de")?)), ("en".into(), Json::Str(f.str("en")?))]);
    let mut j = vec![
        ("key".to_string(), Json::Str(key.clone())),
        ("schema".to_string(), Json::Obj(schema)),
        ("default".to_string(), default),
        ("text".to_string(), text),
    ];
    for k in ["restart", "safety"] {
        if let Some(v) = f.opt_str(k)? {
            j.push((k.to_string(), Json::Str(v)));
        }
    }
    KeyDef::from_json(&Json::Obj(j), &format!("app.{pkg}")).map_err(|e| f.err(format!("setting {key}: {e}")))
}

/// For `perm.rs`: an absolute path a permission names.
pub(crate) fn abs_path(f: &Fields, p: &str) -> Result<(), String> {
    f.wrap(check_abs_path(p))
}
