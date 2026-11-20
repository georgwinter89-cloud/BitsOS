//! SYS-A: "Was hat es gelesen und verbunden?" of the task manager opens the page "Wer hat was gemacht" for one
//! program (`einstellungen/werhatwas/<name>`): the page keeps only the programs whose command or path ends in that
//! name, and says so in its first row.

use crate::werhatwas::{ADDRESS, Data};

/// The program an address names (`einstellungen/werhatwas/brave` -> `brave`).
pub fn program_of(address: &str) -> Option<String> {
    let a = crate::address::normalise(address);
    let rest = a.trim_end_matches('/').strip_prefix(ADDRESS)?.strip_prefix('/')?;
    (!rest.is_empty()).then(|| rest.to_string())
}

/// Whether bitsaccess's program (`app` its path, `name` its command) is `want`.
pub fn same(app: &str, name: &str, want: &str) -> bool {
    let w = want.to_lowercase();
    name.to_lowercase() == w || app.rsplit('/').next().is_some_and(|b| b.to_lowercase() == w)
}

/// `d` with only `want`'s programs.
pub fn only(d: &Data, want: Option<&str>) -> Data {
    let Some(w) = want else { return d.clone() };
    let mut out = d.clone();
    out.programs.retain(|p| same(&p.app, &p.name, w));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::werhatwas::Prog;

    #[test]
    fn the_address_names_one_program_and_the_page_keeps_it() {
        assert_eq!(program_of("einstellungen/werhatwas/brave"), Some("brave".into()));
        assert_eq!(program_of("settings/werhatwas/Brave/"), Some("brave".into()));
        assert_eq!(program_of("einstellungen/werhatwas"), None);
        assert_eq!(program_of("werhatwas"), None);
        assert_eq!(crate::page_kind::PageKind::of_address("einstellungen/werhatwas/brave"), crate::page_kind::PageKind::WerHatWas);
        let p = |app: &str, name: &str| Prog { app: app.into(), name: name.into(), ..Prog::default() };
        let d = Data { loaded: true, programs: vec![p("/apps/brave/brave", "brave"), p("/bin/foot", "foot")], ..Data::default() };
        assert_eq!(only(&d, Some("brave")).programs.len(), 1);
        assert_eq!(only(&d, Some("foot")).programs[0].name, "foot");
        assert_eq!(only(&d, None).programs.len(), 2);
        assert!(only(&d, Some("steam")).programs.is_empty());
    }
}
