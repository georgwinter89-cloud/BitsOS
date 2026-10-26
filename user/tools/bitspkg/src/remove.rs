//! remove and gc. A removal first renames what goes into the trash (one step, so the package is either there
//! or gone), then takes back what the package registered elsewhere (settings keys, permission rules, actions:
//! hooks.rs, PK-2), then deletes; `Store::recover` finishes a removal a cut interrupted.

use crate::fsx;
use crate::install::gc_versions;
use crate::store::{Store, manifests_in};

pub struct Removed {
    pub versions: Vec<String>,
    pub data_removed: bool,
    pub freed: u64,
}

pub fn remove(st: &Store, name: &str, keep_data: bool) -> Result<Removed, String> {
    crate::names::check_name(name)?;
    let _lock = st.lock()?;
    let pkg = st.pkg(name);
    if !pkg.is_dir() {
        return Err(format!("{name} is not installed"));
    }
    let versions = st.versions(name)?;
    // Refused while nothing has changed when a hook could not finish (settingsd down: its keys would stay).
    st.hooks.ready(&manifests_in(&pkg))?;
    let t = st.trash_name(if keep_data { "keep" } else { "drop" }, name);
    fsx::rename(&pkg, &t)?;
    let data_removed = !keep_data && st.data_dir(name).exists();
    if !keep_data {
        st.trash_data(name)?;
    }
    if let Err(e) = st.hooks.remove(name, &manifests_in(&t), keep_data) {
        return Err(format!("{name} is removed, but {e}; the next bitspkg command takes the rest back (its trash entry stays until then)"));
    }
    fsx::remove_tree(&t)?;
    let freed = st.gc_content_after_commit();
    Ok(Removed { versions, data_removed, freed })
}

/// Removes one version; the current one is refused (switch or roll back first).
pub fn remove_version(st: &Store, name: &str, v: &str) -> Result<u64, String> {
    crate::names::check_name(name)?;
    crate::names::check_version(v)?;
    let _lock = st.lock()?;
    if !st.version_dir(name, v).is_dir() {
        return Err(format!("{name} {v} is not installed"));
    }
    if st.current(name).as_deref() == Some(v) {
        return Err(format!("{name} {v} is the current version; switch to another one first"));
    }
    if st.previous(name).as_deref() == Some(v) {
        let p = st.pkg(name).join("previous");
        crate::cut::step("remove previous link");
        std::fs::remove_file(&p).map_err(|e| fsx::err(&p, e))?;
        fsx::sync_parent(&p)?;
    }
    let t = st.trash_name("ver", name);
    fsx::move_dir(&st.version_dir(name, v), &t)?;
    fsx::remove_tree(&t)?;
    Ok(st.gc_content_after_commit())
}

pub struct Collected {
    pub versions: Vec<(String, String)>,
    pub empty_packages: Vec<String>,
    pub freed: u64,
}

/// Versions that are neither current nor previous, packages a cut left without any version (and their data
/// folder only when it is still empty), store files nobody uses, and everything `recover` finds.
pub fn gc(st: &Store) -> Result<Collected, String> {
    let _lock = st.lock()?;
    let mut c = Collected { versions: Vec::new(), empty_packages: Vec::new(), freed: 0 };
    for name in st.packages()? {
        for v in gc_versions(st, &name)? {
            c.versions.push((name.clone(), v));
        }
        if st.versions(&name)?.is_empty() {
            let t = st.trash_name("keep", &name);
            fsx::rename(&st.pkg(&name), &t)?;
            fsx::remove_tree(&t)?;
            let d = st.data_dir(&name);
            if std::fs::read_dir(&d).map(|mut r| r.next().is_none()).unwrap_or(false) {
                fsx::remove_tree(&d)?;
            }
            c.empty_packages.push(name);
        }
    }
    c.freed = st.gc_content()?;
    Ok(c)
}
