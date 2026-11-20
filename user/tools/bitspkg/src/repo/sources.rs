//! Where packages come from (plan 1.2, decisions D1-D3): repositories in pacman's order, CachyOS before Arch, each
//! with its servers, the SigLevel of its database and packages, and its keyring. The servers are URLs with
//! `$repo` and `$arch` (pacman/src/pacman/conf.c:804 replace_server_vars: `$arch` first, then `$repo`, every
//! occurrence) or `include:<file>`, a pacman mirror list in `etc/pacman.d/` (conf.c:1206 process_include; only
//! its `Server =` lines count). Values come from settingsd's `pkg.sources.*` and `pkg.cpu_level` keys, which
//! bitspkg registers (bitspkgd takes them over, PK-6f); without settingsd the defaults below hold. A sources file
//! (`--sources FILE`) replaces both, for tests and for a hand-made setup.

use crate::manifest::Setting;
use crate::settings::Settings;
use crate::toml::{self, Value as T};
use settingsd::json::Json;
use settingsd::key::{Applier, KeyDef, Restart, Safety};
use settingsd::value::{Type, Value};
use std::path::{Path, PathBuf};

/// pacman's `Architecture = auto` on this machine (conf.c:669, uname's machine).
pub const ARCH: &str = "x86_64";
/// The `%ARCH%` values a package may carry at `level` (sync.c accepts only `any` and pacman.conf's
/// Architecture list; CachyOS's pacman fills `auto` with the CPU's levels, the cachyos-v3 databases hold
/// `x86_64_v3` packages beside `x86_64` and `any`). PK-6c refuses a package outside this list. The names for
/// znver4 were not checked against CachyOS (no network): its databases are not in D1's defaults.
pub fn arches(level: Level) -> Vec<&'static str> {
    let mut v = vec!["any", ARCH];
    if level >= Level::V3 {
        v.push("x86_64_v3");
    }
    if level >= Level::V4 {
        v.push("x86_64_v4");
    }
    v
}
pub const ORDER_DEFAULT: &str = "cachyos-$level cachyos-extra-$level cachyos-core-$level cachyos core extra";
const TEXT_MAX: usize = 64 << 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Generic,
    V3,
    V4,
    Znver4,
}

impl Level {
    pub fn parse(s: &str) -> Option<Level> {
        Some(match s {
            "generic" => Level::Generic,
            "v3" => Level::V3,
            "v4" => Level::V4,
            "znver4" => Level::Znver4,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Level::Generic => "generic",
            Level::V3 => "v3",
            Level::V4 => "v4",
            Level::Znver4 => "znver4",
        }
    }
    fn de(self) -> &'static str {
        match self {
            Level::Generic => "x86-64 (jede CPU)",
            Level::V3 => "x86-64-v3 (AVX2)",
            Level::V4 => "x86-64-v4 (AVX-512)",
            Level::Znver4 => "Zen 4 (AVX-512, AMD)",
        }
    }
    /// The level a repository's packages are built for, by CachyOS's naming (`-v3`, `-v4`, `-znver4`).
    pub fn of_repo(name: &str) -> Level {
        if name.ends_with("-znver4") {
            Level::Znver4
        } else if name.ends_with("-v4") {
            Level::V4
        } else if name.ends_with("-v3") {
            Level::V3
        } else {
            Level::Generic
        }
    }
}

/// What this CPU (and the kernel's saved state: AVX needs XSAVE enabled) can run, by CPUID through std's
/// feature detection (no unsafe here). x86-64-v3 and v4 as the psABI defines them (glibc's
/// sysdeps/x86/get-isa-level.h); lahf_lm and the v2 set come with every CPU that has AVX2.
pub fn cpu_can(l: Level) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::is_x86_feature_detected as has;
        let v2 = has!("cmpxchg16b") && has!("popcnt") && has!("sse3") && has!("sse4.1") && has!("sse4.2") && has!("ssse3");
        let v3 = v2 && has!("avx") && has!("avx2") && has!("bmi1") && has!("bmi2") && has!("f16c") && has!("fma") && has!("lzcnt") && has!("movbe") && has!("xsave");
        let v4 = v3 && has!("avx512f") && has!("avx512bw") && has!("avx512cd") && has!("avx512dq") && has!("avx512vl");
        match l {
            Level::Generic => true,
            Level::V3 => v3,
            Level::V4 => v4,
            // Zen 4 builds use -march=znver4: v4 plus the AVX-512 extensions Zen 4 has.
            Level::Znver4 => v4 && has!("avx512vbmi") && has!("avx512vbmi2") && has!("avx512bitalg") && has!("avx512vpopcntdq") && has!("avx512bf16"),
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        l == Level::Generic
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sig {
    Required,
    Optional,
}

impl Sig {
    fn parse(s: &str) -> Option<Sig> {
        match s {
            "required" => Some(Sig::Required),
            "optional" => Some(Sig::Optional),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Sig::Required => "required",
            Sig::Optional => "optional",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    pub name: String,
    /// As configured: URLs with `$repo`/`$arch`, or `include:<file>`.
    pub servers: Vec<String>,
    pub sig_db: Sig,
    /// Always Required: a package without a good signature is never installed (plan 1.4).
    pub sig_pkg: Sig,
    pub keyring: String,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub level: Level,
    pub sources: Vec<Source>,
    /// Where the values came from (for the report): "settingsd", "defaults (...)", "file ...".
    pub origin: String,
    /// D4 (`pkg.space.max_gb`): how much programs may take, in GB; None = half of /data.
    pub space_max_gb: Option<u64>,
    /// D5 (PK-6e): how often to look for updates and what to do with what was found.
    pub updates: Updates,
}

/// `pkg.updates.check`: how often bitspkgd's timer runs `bitspkg update --check` (D5: daily).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Check {
    #[default]
    Daily,
    Weekly,
    Never,
}

/// `pkg.updates.install`: D5, never on their own: what is found waits for Georg's click. `Night` is D5's
/// alternative (installed at night, the old version kept), offered as a setting, not the default.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Install {
    #[default]
    Ask,
    Night,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Updates {
    pub check: Check,
    pub install: Install,
}

pub const CHECK_KEY: &str = "pkg.updates.check";
pub const INSTALL_KEY: &str = "pkg.updates.install";
pub const CHECK_VALUES: [&str; 3] = ["daily", "weekly", "never"];
pub const INSTALL_VALUES: [&str; 2] = ["ask", "night"];
/// Plan 1.10 (PK-6f): whether a plan lists the Zusätze (optional dependencies) for the page to ask about.
pub const OPTIONAL_KEY: &str = "pkg.optional.ask";

impl Check {
    pub fn parse(s: &str) -> Option<Check> {
        Some(match s {
            "daily" => Check::Daily,
            "weekly" => Check::Weekly,
            "never" => Check::Never,
            _ => return None,
        })
    }
}

impl Install {
    pub fn parse(s: &str) -> Option<Install> {
        Some(match s {
            "ask" => Install::Ask,
            "night" => Install::Night,
            _ => return None,
        })
    }
}

pub fn check_repo_name(n: &str) -> Result<(), String> {
    if n.is_empty() || n.len() > 64 || n.starts_with(['-', '.']) || !n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_') {
        return Err(format!("ungültiger Quellenname {n:?} (Kleinbuchstaben, Ziffern, - und _)"));
    }
    Ok(())
}

/// D1's defaults for a repository name.
pub fn default_source(name: &str, level: Level) -> Source {
    let (servers, sig_db, keyring) = if name.starts_with("cachyos") {
        let list = match Level::of_repo(name) {
            Level::Generic => "cachyos-mirrorlist",
            Level::V3 => "cachyos-v3-mirrorlist",
            Level::V4 | Level::Znver4 => "cachyos-v4-mirrorlist",
        };
        (vec![format!("include:{list}")], Sig::Required, "cachyos")
    } else if matches!(name, "core" | "extra") {
        (vec!["include:mirrorlist".to_string()], Sig::Optional, "archlinux")
    } else {
        (Vec::new(), Sig::Required, "archlinux")
    };
    let _ = level;
    Source { name: name.to_string(), servers, sig_db, sig_pkg: Sig::Required, keyring: keyring.to_string() }
}

/// The order text with `$level` filled in; with level generic the `$level` entries are dropped (there are no
/// generic `cachyos-core`/`-extra` repositories; `cachyos` itself is generic).
pub fn expand_order(order: &str, level: Level) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for w in order.split_whitespace() {
        if w.contains("$level") && level == Level::Generic {
            continue;
        }
        let n = w.replace("$level", level.name());
        check_repo_name(&n)?;
        if Level::of_repo(&n) == Level::Generic && n.contains("$") {
            return Err(format!("{w}: unbekannte Variable"));
        }
        // D3 and plan 1.2: never multilib (32 bit), never testing.
        if n == "multilib" || n.contains("testing") || n == "aur" {
            return Err(format!("Quelle {n} ist ausgeschlossen (kein 32-Bit, keine Testversionen, kein AUR)"));
        }
        if out.contains(&n) {
            return Err(format!("Quelle {n} steht zweimal in der Reihenfolge"));
        }
        out.push(n);
    }
    Ok(out)
}

pub fn defaults(origin: &str) -> Config {
    let level = Level::V3;
    let sources = expand_order(ORDER_DEFAULT, level).unwrap_or_default().iter().map(|n| default_source(n, level)).collect();
    Config { level, sources, origin: origin.to_string(), space_max_gb: None, updates: Updates::default() }
}

fn split_servers(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

// ---- settingsd ----

fn key(name: &str, ty: Type, default: Value, de: &str, en: &str) -> Setting {
    let def = KeyDef {
        name: name.to_string(),
        ty,
        default,
        de: de.to_string(),
        en: en.to_string(),
        applier: Applier::Service(crate::settings::OWNER.to_string()),
        restart: Restart::None,
        safety: Safety::Normal,
        owner: crate::settings::OWNER.to_string(),
    };
    Setting { def, table: crate::toml::Table::default() }
}

fn choice(values: &[&str], de: &[&str]) -> Type {
    Type::Choice { values: values.iter().map(|s| s.to_string()).collect(), de: de.iter().map(|s| s.to_string()).collect() }
}

/// D4: the space programs may take.
pub const SPACE_KEY: &str = "pkg.space.max_gb";
/// The word that stands for "half of /data" (D4's default).
pub const SPACE_AUTO: &str = "auto";

/// The keys of `pkg.cpu_level`, `pkg.sources.order`, `pkg.space.max_gb`, `pkg.updates.*`, `pkg.optional.ask` and every source of `order` (with D1's
/// defaults).
pub fn keys(names: &[String], level: Level) -> Vec<Setting> {
    let levels = [Level::Generic, Level::V3, Level::V4, Level::Znver4];
    let mut v = vec![
        key(
            SPACE_KEY,
            Type::Int { min: 0, max: 1 << 20, unit: "GB".into(), word: Some(SPACE_AUTO.into()) },
            Value::Text(SPACE_AUTO.into()),
            "Höchstens so viel Platz dürfen Programme auf /data belegen (auto: die Hälfte von /data)",
            "At most this much of /data for programs (auto: half of /data)",
        ),
        key(
            CHECK_KEY,
            choice(&CHECK_VALUES, &["täglich", "wöchentlich", "nie"]),
            Value::Text("daily".into()),
            "Wie oft nach Aktualisierungen geschaut wird (es wird nur nachgesehen, nichts installiert)",
            "How often to look for updates (only looked for, nothing installed)",
        ),
        key(
            INSTALL_KEY,
            choice(&INSTALL_VALUES, &["fragen", "nachts selbst (die alte Fassung bleibt)"]),
            Value::Text("ask".into()),
            "Was mit gefundenen Aktualisierungen geschieht",
            "What happens with updates that were found",
        ),
        key(
            OPTIONAL_KEY,
            Type::Bool,
            Value::Bool(true),
            "Vor dem Installieren die Zusätze (optionale Teile) zur Auswahl zeigen",
            "Show the extras (optional parts) to choose from before installing",
        ),
        key(
            "pkg.cpu_level",
            choice(&levels.map(Level::name), &levels.map(Level::de)),
            Value::Text("v3".into()),
            "Fassung der CachyOS-Programme für diesen Prozessor (v3 läuft auf allen modernen CPUs)",
            "CPU level of the CachyOS packages (v3 runs on every modern CPU)",
        ),
        key(
            "pkg.sources.order",
            Type::Text { max: TEXT_MAX },
            Value::Text(ORDER_DEFAULT.into()),
            "Quellen in ihrer Reihenfolge (die erste mit einem Namen gewinnt)",
            "Repositories in order (the first with a name wins)",
        ),
    ];
    for n in names {
        let d = default_source(n, level);
        v.push(key(
            &format!("pkg.sources.{n}.servers"),
            Type::Text { max: TEXT_MAX },
            Value::Text(d.servers.join(" ")),
            &format!("Server der Quelle {n} (Adressen mit $repo und $arch, oder include:<Spiegelliste>)"),
            &format!("Servers of {n} (URLs with $repo and $arch, or include:<mirror list>)"),
        ));
        v.push(key(
            &format!("pkg.sources.{n}.sig_db"),
            choice(&["required", "optional"], &["Pflicht", "wenn vorhanden"]),
            Value::Text(d.sig_db.name().into()),
            &format!("Signatur der Datenbank von {n}"),
            &format!("Signature of {n}'s database"),
        ));
        v.push(key(
            &format!("pkg.sources.{n}.sig_pkg"),
            choice(&["required"], &["Pflicht"]),
            Value::Text("required".into()),
            &format!("Signatur der Pakete von {n} (immer Pflicht)"),
            &format!("Signature of {n}'s packages (always required)"),
        ));
        v.push(key(
            &format!("pkg.sources.{n}.keyring"),
            choice(&["archlinux", "cachyos"], &["Arch Linux", "CachyOS"]),
            Value::Text(d.keyring.clone()),
            &format!("Schlüsselbund, der {n} prüft"),
            &format!("Keyring that checks {n}"),
        ));
    }
    v
}

fn get(s: &Settings, k: &str) -> Result<Option<String>, String> {
    match s.request(&Json::obj().with("op", "get").with("key", k))? {
        Ok(j) => Ok(j.str("value").map(str::to_string)),
        Err((c, _)) if c == "unknown_key" => Ok(None),
        Err((c, m)) => Err(format!("settingsd: {k}: {c} {m}")),
    }
}

/// Registers the keys (first the two that name the sources, then those of each source) and reads them back.
pub fn from_settings(s: &Settings) -> Result<Config, String> {
    s.register(&keys(&[], Level::V3))?;
    let level_text = get(s, "pkg.cpu_level")?.unwrap_or_else(|| "v3".into());
    let level = Level::parse(&level_text).ok_or_else(|| format!("pkg.cpu_level {level_text:?} unbekannt"))?;
    let order = get(s, "pkg.sources.order")?.unwrap_or_else(|| ORDER_DEFAULT.into());
    let names = expand_order(&order, level)?;
    s.register(&keys(&names, level))?;
    let mut sources = Vec::new();
    for n in &names {
        let d = default_source(n, level);
        let servers = get(s, &format!("pkg.sources.{n}.servers"))?.map(|t| split_servers(&t)).unwrap_or(d.servers);
        let sig_db = get(s, &format!("pkg.sources.{n}.sig_db"))?.and_then(|t| Sig::parse(&t)).unwrap_or(d.sig_db);
        let keyring = get(s, &format!("pkg.sources.{n}.keyring"))?.unwrap_or(d.keyring);
        sources.push(Source { name: n.clone(), servers, sig_db, sig_pkg: Sig::Required, keyring });
    }
    let space_max_gb = match s.request(&Json::obj().with("op", "get").with("key", SPACE_KEY))? {
        Ok(j) => match j.get("value") {
            Some(Json::Int(n)) if *n >= 0 => Some(*n as u64),
            _ => None,
        },
        Err(_) => None,
    };
    let updates = Updates {
        check: get(s, CHECK_KEY)?.and_then(|t| Check::parse(&t)).unwrap_or_default(),
        install: get(s, INSTALL_KEY)?.and_then(|t| Install::parse(&t)).unwrap_or_default(),
    };
    Ok(Config { level, sources, origin: "settingsd".into(), space_max_gb, updates })
}

// ---- a sources file ----

/// `cpu_level = "v3"` and `[[source]]` tables with `name`, `servers` (list), `sig_db`, `keyring`.
pub fn from_file(p: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
    let doc = toml::parse(&text).map_err(|e| format!("{}: {e}", p.display()))?;
    let str_of = |v: Option<&T>| match v {
        Some(T::Str(s)) => Some(s.clone()),
        _ => None,
    };
    let level_text = str_of(doc.root.get("cpu_level")).unwrap_or_else(|| "v3".into());
    let level = Level::parse(&level_text).ok_or_else(|| format!("cpu_level {level_text:?} unbekannt"))?;
    let mut sources: Vec<Source> = Vec::new();
    for (h, t) in &doc.arrays {
        if h != "source" {
            return Err(format!("{}: unbekannte Tabelle [[{h}]]", p.display()));
        }
        let name = str_of(t.get("name")).ok_or_else(|| format!("{}: Zeile {}: name fehlt", p.display(), t.line))?;
        check_repo_name(&name)?;
        let d = default_source(&name, level);
        let servers = match t.get("servers") {
            Some(T::Arr(a)) => a.iter().filter_map(|v| if let T::Str(s) = v { Some(s.clone()) } else { None }).collect(),
            Some(T::Str(s)) => split_servers(s),
            _ => d.servers,
        };
        let sig_db = match str_of(t.get("sig_db")) {
            Some(x) => Sig::parse(&x).ok_or_else(|| format!("sig_db {x:?}: required oder optional"))?,
            None => d.sig_db,
        };
        let keyring = str_of(t.get("keyring")).unwrap_or(d.keyring);
        if sources.iter().any(|s| s.name == name) {
            return Err(format!("Quelle {name} doppelt"));
        }
        sources.push(Source { name, servers, sig_db, sig_pkg: Sig::Required, keyring });
    }
    // D4 in a sources file (tests, an admin's file): `space_max_gb = N`; absent = half of /data.
    let space_max_gb = match doc.root.get("space_max_gb") {
        Some(T::Int(n)) if *n >= 0 => Some(*n as u64),
        Some(_) => return Err(format!("{}: space_max_gb muss eine Zahl ab 0 sein", p.display())),
        None => None,
    };
    // D5 in a sources file: `updates_check = "daily"`, `updates_install = "ask"`.
    let mut updates = Updates::default();
    if let Some(x) = str_of(doc.root.get("updates_check")) {
        updates.check = Check::parse(&x).ok_or_else(|| format!("updates_check {x:?}: daily, weekly oder never"))?;
    }
    if let Some(x) = str_of(doc.root.get("updates_install")) {
        updates.install = Install::parse(&x).ok_or_else(|| format!("updates_install {x:?}: ask oder night"))?;
    }
    Ok(Config { level, sources, origin: format!("file {}", p.display()), space_max_gb, updates })
}

// ---- servers ----

/// The `Server =` URLs of a pacman mirror list (comments and other lines skipped, as conf.c's ini reader does
/// for an Include inside a repository section).
pub fn mirrorlist(text: &str) -> Vec<String> {
    let mut v = Vec::new();
    for l in text.lines() {
        let l = l.split('#').next().unwrap_or("").trim();
        if let Some((k, val)) = l.split_once('=')
            && k.trim() == "Server"
        {
            let u = val.trim();
            if !u.is_empty() && !v.iter().any(|x| x == u) {
                v.push(u.to_string());
            }
        }
    }
    v
}

pub fn fill(url: &str, repo: &str) -> String {
    url.replace("$arch", ARCH).replace("$repo", repo)
}

/// The URLs to try for `src`, in order: includes read from `mirror_dir`, `$arch` and `$repo` filled in, only
/// http(s). Problems (a missing list, a bad entry) are returned beside the list: they go into the report.
pub fn servers(src: &Source, mirror_dir: &Path) -> (Vec<String>, Vec<String>) {
    let mut urls: Vec<String> = Vec::new();
    let mut problems = Vec::new();
    for s in &src.servers {
        let templates = if let Some(f) = s.strip_prefix("include:") {
            if f.is_empty() || f.contains('/') || f.starts_with('.') {
                problems.push(format!("{s}: nur ein Dateiname in etc/pacman.d"));
                continue;
            }
            let p: PathBuf = mirror_dir.join(f);
            match std::fs::read(&p) {
                Ok(b) if b.len() <= TEXT_MAX * 16 => mirrorlist(&String::from_utf8_lossy(&b)),
                Ok(_) => {
                    problems.push(format!("{}: Spiegelliste zu groß", p.display()));
                    continue;
                }
                Err(e) => {
                    problems.push(format!("Spiegelliste {} fehlt: {e}", p.display()));
                    continue;
                }
            }
        } else {
            vec![s.clone()]
        };
        for t in templates {
            let u = fill(&t, &src.name);
            if !(u.starts_with("https://") || u.starts_with("http://")) || u.bytes().any(|b| b <= b' ') {
                problems.push(format!("{t}: keine http(s)-Adresse"));
                continue;
            }
            if !urls.contains(&u) {
                urls.push(u);
            }
        }
    }
    (urls, problems)
}
