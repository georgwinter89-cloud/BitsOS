//! A dependency string and pacman's comparisons (pacman/lib/libalpm/deps.c:458 alpm_dep_from_string, :392
//! dep_vercmp, :413 _alpm_depcmp_literal, :428 _alpm_depcmp_provides, :452 _alpm_depcmp). The parse is pacman's
//! (a description after ": ", then the first '<', else the first '>', else the first '='), with refusals where
//! pacman would take nonsense: an empty or invalid name, an operator without a version, a version with blanks,
//! control characters or another operator in it, a string over 64 KiB.

use crate::repo::syncdb::{self, FIELD_MAX};
use crate::repo::vercmp::vercmp;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mod {
    Any,
    Eq,
    Ge,
    Le,
    Lt,
    Gt,
}

impl Mod {
    pub fn text(self) -> &'static str {
        match self {
            Mod::Any => "",
            Mod::Eq => "=",
            Mod::Ge => ">=",
            Mod::Le => "<=",
            Mod::Lt => "<",
            Mod::Gt => ">",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dep {
    pub name: String,
    pub m: Mod,
    pub version: String,
    /// The reason of an optional dependency ("name: reason").
    pub desc: Option<String>,
}

impl Dep {
    pub fn any(name: &str) -> Dep {
        Dep { name: name.to_string(), m: Mod::Any, version: String::new(), desc: None }
    }
    /// alpm_dep_compute_string without the description.
    pub fn text(&self) -> String {
        format!("{}{}{}", self.name, self.m.text(), self.version)
    }
}

pub fn parse(s: &str) -> Result<Dep, String> {
    if s.len() > FIELD_MAX {
        return Err(format!("Abhängigkeit länger als 64 KiB ({} Bytes)", s.len()));
    }
    if s.chars().any(char::is_control) {
        return Err(format!("Abhängigkeit {:?} enthält Steuerzeichen", short(s)));
    }
    // ": " with the blank, so an epoch ("1:2.3") is not taken for a description (deps.c:471).
    let (body, desc) = match s.find(": ") {
        Some(i) => (&s[..i], Some(s[i + 2..].trim().to_string())),
        None => (s, None),
    };
    let b = body.as_bytes();
    let (m, at, from) = if let Some(i) = body.find('<') {
        if b.get(i + 1) == Some(&b'=') { (Mod::Le, i, i + 2) } else { (Mod::Lt, i, i + 1) }
    } else if let Some(i) = body.find('>') {
        if b.get(i + 1) == Some(&b'=') { (Mod::Ge, i, i + 2) } else { (Mod::Gt, i, i + 1) }
    } else if let Some(i) = body.find('=') {
        (Mod::Eq, i, i + 1)
    } else {
        (Mod::Any, body.len(), body.len())
    };
    let name = &body[..at];
    let version = &body[from..];
    syncdb::check_name(name).map_err(|e| format!("Abhängigkeit {:?}: {e}", short(s)))?;
    if m != Mod::Any && (version.is_empty() || version.bytes().any(|c| c <= b' ' || b"<>=".contains(&c))) {
        return Err(format!("Abhängigkeit {:?}: Version fehlt oder ist ungültig", short(s)));
    }
    Ok(Dep { name: name.to_string(), m, version: version.to_string(), desc })
}

/// At most 80 characters of `s` for a message.
pub fn short(s: &str) -> String {
    let mut t: String = s.chars().take(80).collect();
    if t.len() < s.len() {
        t.push('…');
    }
    t
}

/// deps.c:392 dep_vercmp.
pub fn ver_ok(have: &str, m: Mod, want: &str) -> bool {
    if m == Mod::Any {
        return true;
    }
    let c = vercmp(have, want);
    match m {
        Mod::Any => true,
        Mod::Eq => c == 0,
        Mod::Ge => c >= 0,
        Mod::Le => c <= 0,
        Mod::Lt => c < 0,
        Mod::Gt => c > 0,
    }
}

/// deps.c:413: the package's own name and version.
pub fn literal(name: &str, version: &str, d: &Dep) -> bool {
    name == d.name && ver_ok(version, d.m, &d.version)
}

/// deps.c:428: a provision without a version satisfies only an unversioned dependency; a versioned one
/// (`provision->mod == EQ`, e.g. the soname `libcurl.so=4-64`) is compared.
pub fn provides(d: &Dep, provs: &[Dep]) -> bool {
    provs.iter().any(|p| {
        p.name == d.name
            && match d.m {
                Mod::Any => true,
                _ => p.m == Mod::Eq && ver_ok(&p.version, d.m, &d.version),
            }
    })
}
