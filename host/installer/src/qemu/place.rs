//! Placing the launchers (the start script, Linux's desktop entries, Windows' shortcuts) and taking everything
//! away again with `--uninstall`.

use super::args::{self, Os, Vm};
use super::files::Layout;
use super::setup::{self, OS};
use crate::{release, ui};
use std::path::{Path, PathBuf};

/// The start script, the desktop entries (Linux) or the shortcuts (Windows); returns the shortcuts made.
pub fn launchers(l: &Layout, vm: &Vm) -> Result<Vec<PathBuf>, String> {
    let script = l.script();
    let text = match OS {
        Os::Windows => args::cmd_script(vm, &l.log(), release::VERSION)?,
        Os::Linux => args::shell_script(vm, &l.log(), release::VERSION),
    };
    std::fs::write(&script, text).map_err(|e| format!("{}: {e}", script.display()))?;
    l.remember(&script)?;
    let made = if OS == Os::Windows { setup::windows_shortcuts(&script, &l.dir, &vm.qemu) } else { desktop_entries(&script)? };
    for p in &made {
        l.remember(p)?;
    }
    Ok(made)
}

#[cfg(unix)]
fn desktop_entries(script: &Path) -> Result<Vec<PathBuf>, String> {
    use super::files;
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &Path, m: u32| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m));
    mode(script, 0o755).map_err(|e| format!("{}: {e}", script.display()))?;
    let env = |k: &str| std::env::var(k).ok();
    let entry = args::desktop_entry(script);
    let mut made = Vec::new();
    let apps = files::data_home(OS, &env).map(|d| d.join("applications"));
    let desk = files::desktop_dir(&env);
    for d in apps.iter().chain(desk.iter()) {
        let p = d.join("bitsos.desktop");
        if std::fs::create_dir_all(d).and_then(|_| std::fs::write(&p, &entry)).and_then(|_| mode(&p, 0o755)).is_ok() {
            made.push(p);
        }
    }
    // GNOME starts a launcher on the desktop only once it is marked trusted; elsewhere this does nothing.
    if let Some(p) = desk.map(|d| d.join("bitsos.desktop")).filter(|p| made.contains(p)) {
        let _ = std::process::Command::new("gio").arg("set").arg(&p).args(["metadata::trusted", "true"]).stderr(std::process::Stdio::null()).status();
    }
    Ok(made)
}

#[cfg(not(unix))]
fn desktop_entries(_: &Path) -> Result<Vec<PathBuf>, String> {
    Ok(Vec::new())
}

/// Lists what QEMU mode made and removes it after the person's yes (or `--yes`); `dry_run` only lists.
pub fn uninstall(l: &Layout, yes: bool, dry_run: bool) -> Result<(), String> {
    let list = l.to_remove();
    if list.is_empty() {
        println!("Nothing of BitsOS's QEMU mode is in {}.", l.dir.display());
        return Ok(());
    }
    println!("These files were made by bitsos-install --qemu and {} (the image with everything in BitsOS's data):", if dry_run { "would be removed" } else { "are removed" });
    for p in &list {
        println!("  {}", p.display());
    }
    if dry_run {
        println!("dry run: nothing was removed.");
        return Ok(());
    }
    if !yes && !ui::yes("Remove them?", false)? {
        return Err("stopped; nothing was removed".into());
    }
    for p in &list {
        std::fs::remove_file(p).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    let _ = std::fs::remove_dir(&l.dir);
    println!("Removed {} files. The release downloads stay in bitsos-install's cache folder (default {}).", list.len(), crate::fetch::default_cache().display());
    Ok(())
}
