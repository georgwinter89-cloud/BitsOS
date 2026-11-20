//! The registry's part of AT-SPI that programs need on start: `Socket.Embed` (a program announces its root; the
//! answer is the desktop it hangs under) and `Registry.GetRegisteredEvents` (which events anyone listens to: none,
//! so programs send no event traffic). The programs found that way, or by asking the bus, by process.

use crate::atspi::{self, Bus};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use zbus::message::Header;
use zbus::{Connection, OwnedObjectPath, Type, interface};

/// An object reference as AT-SPI sends it: (bus name, path).
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq)]
pub struct Ref(pub String, pub OwnedObjectPath);

#[derive(Debug, Clone, PartialEq)]
pub struct App {
    pub bus: String,
    pub root: String,
    /// The bus client is the asked process itself; false: a process it started (see `find`).
    pub exact: bool,
}

/// Programs by bus name (root path), and the process behind each bus name.
#[derive(Default)]
pub struct Apps {
    roots: Mutex<HashMap<String, String>>,
    pids: Mutex<HashMap<String, u32>>,
}

impl Apps {
    pub fn embed(&self, bus: &str, root: &str) {
        self.roots.lock().unwrap_or_else(|e| e.into_inner()).insert(bus.to_string(), root.to_string());
    }

    pub fn forget(&self, bus: &str) {
        self.roots.lock().unwrap_or_else(|e| e.into_inner()).remove(bus);
        self.pids.lock().unwrap_or_else(|e| e.into_inner()).remove(bus);
    }

    fn root(&self, bus: &str) -> Option<String> {
        self.roots.lock().unwrap_or_else(|e| e.into_inner()).get(bus).cloned()
    }

    pub fn count(&self) -> usize {
        self.roots.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

pub struct Socket {
    pub apps: Arc<Apps>,
}

#[interface(name = "org.a11y.atspi.Socket")]
impl Socket {
    /// The answer is one `(so)` argument: atk-bridge checks that signature (a bare `Ref` would go out as `so`).
    fn embed(&self, #[zbus(header)] hdr: Header<'_>, #[zbus(connection)] conn: &Connection, plug: Ref) -> (Ref,) {
        if let Some(s) = hdr.sender() {
            self.apps.embed(s.as_str(), plug.1.as_str());
        }
        let me = conn.unique_name().map(|n| n.to_string()).unwrap_or_default();
        (Ref(me, OwnedObjectPath::try_from(atspi::ROOT_PATH).expect("a valid path")),)
    }

    fn unembed(&self, #[zbus(header)] hdr: Header<'_>, _plug: Ref) {
        if let Some(s) = hdr.sender() {
            self.apps.forget(s.as_str());
        }
    }
}

pub struct Registry;

#[interface(name = "org.a11y.atspi.Registry")]
impl Registry {
    fn get_registered_events(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// A keystroke listener as at-spi2-registryd lists them; there are never any here.
type Keystroke = (String, OwnedObjectPath, u32, u32, Vec<(i32, i32, String, i32)>, u32, (bool, bool, bool));

/// The device event controller: atk-bridge asks it on start which keystroke listeners there are (none: programs
/// then send no key events to the bus).
pub struct DeviceEvents;

#[interface(name = "org.a11y.atspi.DeviceEventController")]
impl DeviceEvents {
    fn get_keystroke_listeners(&self) -> Vec<Keystroke> {
        Vec::new()
    }
}

const DBUS: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";

async fn pid_of(bus: &Bus, apps: &Apps, name: &str) -> Option<u32> {
    if let Some(p) = apps.pids.lock().unwrap_or_else(|e| e.into_inner()).get(name) {
        return Some(*p);
    }
    let p: u32 = bus.call(DBUS, DBUS_PATH, DBUS, "GetConnectionUnixProcessID", &(name,)).await.ok()?;
    apps.pids.lock().unwrap_or_else(|e| e.into_inner()).insert(name.to_string(), p);
    Some(p)
}

/// The parent process of `pid` from /proc (None at the top or when it is unreadable).
fn ppid(pid: u32) -> Option<u32> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    s.lines().find_map(|l| l.strip_prefix("PPid:")).and_then(|v| v.trim().parse().ok()).filter(|p| *p > 1)
}

/// Whether `a` is `b` or one of its ancestors (a launcher started the program).
fn ancestor(a: u32, b: u32) -> bool {
    let mut p = Some(b);
    for _ in 0..16 {
        match p {
            Some(x) if x == a => return true,
            Some(x) => p = ppid(x),
            None => return false,
        }
    }
    false
}

/// The bus clients that may stand for process `pid`: the process itself first, then processes it started
/// (a launcher's window whose program runs below it). Never an ancestor: a window of a terminal, a game or a
/// viewer must not show the tree of the program that started it. A descendant is only taken by `tree` when the
/// caller's title is exactly one of its windows (c3.rs), so an unrelated child cannot answer either.
pub async fn find(bus: &Bus, apps: &Apps, pid: u32) -> Vec<App> {
    let Ok(names) = bus.call::<_, Vec<String>>(DBUS, DBUS_PATH, DBUS, "ListNames", &()).await else { return Vec::new() };
    let me = bus.conn.unique_name().map(|n| n.to_string()).unwrap_or_default();
    let known: Vec<String> = apps.roots.lock().unwrap_or_else(|e| e.into_inner()).keys().cloned().collect();
    for k in known.iter().filter(|k| !names.contains(k)) {
        apps.forget(k);
    }
    let mut cands = Vec::new();
    for n in names.iter().filter(|n| n.starts_with(':') && **n != me) {
        match pid_of(bus, apps, n).await.filter(|p| *p != std::process::id()) {
            Some(p) if p == pid => cands.insert(0, (n.clone(), true)),
            Some(p) if ancestor(pid, p) => cands.push((n.clone(), false)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for (name, exact) in cands {
        if let Some(root) = root_of(bus, apps, &name).await {
            out.push(App { bus: name, root, exact });
        }
    }
    out
}

/// The program's root: where it embedded itself, else (it started before this registry) where at-spi puts it.
async fn root_of(bus: &Bus, apps: &Apps, name: &str) -> Option<String> {
    if let Some(r) = apps.root(name) {
        return Some(r);
    }
    let r: u32 = bus.call(name, atspi::ROOT_PATH, atspi::ACCESSIBLE, "GetRole", &()).await.ok()?;
    if r != atspi::role::APPLICATION {
        return None;
    }
    apps.embed(name, atspi::ROOT_PATH);
    Some(atspi::ROOT_PATH.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_is_its_own_ancestor_and_init_is_nobodys() {
        let me = std::process::id();
        assert!(ancestor(me, me));
        if let Some(parent) = ppid(me) {
            assert!(ancestor(parent, me));
            assert!(!ancestor(me, parent));
        }
    }
}
