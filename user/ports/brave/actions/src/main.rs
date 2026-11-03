//! brave-act: Brave's actions for Claude (AC-3). Registers brave_open_url, brave_tabs, brave_read_page,
//! brave_screenshot_tab, brave_click_link, brave_fill_form (change) and brave_download (change) as MCP tools
//! (bitsact over mcpreg) and drives Brave over the DevTools protocol on a pipe (--remote-debugging-pipe, Brave's
//! fds 3/4; no TCP port). Brave starts through its launcher at the first call that needs it.
//!
//!   brave-act [--launcher "CMD ARGS"] [--tree DIR] [--download-dir DIR] [--log FILE] [--brave-arg ARG]...
//!             [--name NAME] [--mcp-socket PATH] [--settings-socket PATH]
//!
//! Defaults: launcher `/bin/sh /boot/BITSOS2/bin/brave`, tree `/boot/BITSOS2/brave`, downloads into
//! /data/home/1000/Downloads (or /tmp/Downloads without a data volume), the launcher's output in
//! `$XDG_RUNTIME_DIR/brave-act.log` (or /tmp/brave-act-<uid>/brave-act.log: a folder of mode 0700 that brave-act
//! makes and checks, so no other user can put a link where the log goes). Ends when the kernel closes the
//! registry connection; Brave ends with it (it loses its pipe; SIGTERM/SIGKILL if it does not).
#![forbid(unsafe_code)]

mod actions;
mod browser;
mod cdp;
mod js;

use bitsact::events::{Events, Level, q};
use bitsact::Config;
use browser::{Browser, Settings};
use std::path::PathBuf;
use std::sync::Arc;

fn main() {
    let mut cfg = Config::new(actions::SERVICE);
    let mut launcher = vec!["/bin/sh".to_string(), "/boot/BITSOS2/bin/brave".to_string()];
    let mut tree = PathBuf::from("/boot/BITSOS2/brave");
    let mut dl: Option<PathBuf> = None;
    let mut log: Option<PathBuf> = None;
    let mut extra = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut v = || it.next().unwrap_or_else(|| usage(&format!("{a} wants a value")));
        match a.as_str() {
            "--launcher" => launcher = v().split_whitespace().map(String::from).collect(),
            "--tree" => tree = v().into(),
            "--download-dir" => dl = Some(v().into()),
            "--log" => log = Some(v().into()),
            "--brave-arg" => extra.push(v()),
            "--name" => cfg.service = v(),
            "--mcp-socket" => cfg.mcp_socket = v(),
            "--settings-socket" => cfg.settings_socket = v(),
            "-h" | "--help" => {
                println!("brave-act: Brave's actions for Claude as MCP tools (see the source's head for the options)");
                return;
            }
            _ => usage(&format!("unknown argument {a}")),
        }
    }
    if launcher.is_empty() {
        usage("--launcher is empty");
    }
    let uid = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("Uid:")).and_then(|l| l.split_whitespace().nth(2).map(String::from)))
        .unwrap_or_else(|| "x".into());
    let log = match log {
        Some(l) => l,
        None => match std::env::var("XDG_RUNTIME_DIR") {
            Ok(d) if !d.is_empty() && std::path::Path::new(&d).is_dir() => PathBuf::from(d).join("brave-act.log"),
            _ => private_dir(&PathBuf::from(format!("/tmp/brave-act-{uid}")), uid.parse().ok()).unwrap_or_else(|e| usage(&e)).join("brave-act.log"),
        },
    };
    let dl = dl.unwrap_or_else(|| {
        if std::path::Path::new("/data/home/1000").is_dir() { "/data/home/1000/Downloads".into() } else { "/tmp/Downloads".into() }
    });
    let ev = Events::system(&format!("brave-act[{}]", cfg.service));
    ev.emit(Level::Info, "start", &format!("uid={uid} launcher={} log={} downloads={}", q(&launcher.join(" ")), q(&log.display().to_string()), q(&dl.display().to_string())));
    let browser = Browser::new(Settings { launcher, tree, extra, log, env: Vec::new() }, ev.clone());
    let brave = actions::Brave::new(browser.clone(), dl, ev.clone());
    let r = bitsact::serve(&cfg, actions::list(), Arc::new(brave), ev.clone());
    browser.shutdown();
    if let Err(e) = r {
        ev.emit(Level::Error, "failed", &format!("why={}", q(&e.to_string())));
        eprintln!("brave-act: {e}");
        std::process::exit(1);
    }
}

/// `d` as a folder of mode 0700 that belongs to this user (`me`, the effective uid) and is no symbolic link (made if missing).
fn private_dir(d: &std::path::Path, me: Option<u32>) -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    match std::fs::DirBuilder::new().mode(0o700).create(d) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("{}: {e}", d.display())),
    }
    let m = std::fs::symlink_metadata(d).map_err(|e| format!("{}: {e}", d.display()))?;
    if !m.is_dir() || Some(m.uid()) != me || m.permissions().mode() & 0o077 != 0 {
        return Err(format!("{}: not a private folder of this user (a link, another owner or open to others); give --log", d.display()));
    }
    Ok(d.to_path_buf())
}

fn usage(why: &str) -> ! {
    eprintln!("brave-act: {why} (brave-act --help)");
    std::process::exit(2)
}
