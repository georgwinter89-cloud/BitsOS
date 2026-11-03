//! MC-10: full access for a paired client (Georg 27.10.): set only with real input at the Mini-PC's keyboard,
//! in bitscomp's access dialog (asked by the client's `request_access`, or opened by Georg himself with
//! Strg+Alt+V), for 1 h, 8 h, 24 h or 7 days, bound to the client's key (the SHA-256 of its paired certificate)
//! and, when Georg ticks it, also opening the NVMe write gate for that time. Kept root-only on the data volume
//! (`DIR`, 0700/0600; the kernel's file tools never reach it, `mcp::tools::files::ACCESS_STORE`), so a grant
//! survives Reboot and Reload until its end; Georg revokes it at the keyboard. The kernel gets the table at
//! the gate (`grants`) after every change and judges every grant by the same clock rule
//! (`mcp_wire::access::judge`) itself.
//!
//! The paired clients come from bitsmcp's pairings on the stick (`PAIRED`, root-only): a grant names only a
//! client found there, so neither a confused bitsmcp nor the desk can name a key Georg never paired.

use crate::confirm::{Input, Shown};
use crate::decide::Keep;
use crate::log;
use mcp_wire::access::{self as acc, Ended, Grant, Seen};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Where the grants are kept: the data volume, root-only.
pub const DIR: &str = "/data/bitsos-access";
pub const MOUNT: &str = "/data";
/// bitsmcp's paired client certificates (MC-8b).
pub const PAIRED: &str = "/boot/BITSOS2/mcp/paired";
/// A certificate file bigger than this is no client's (bitsmcp's `cert::CERT_MAX`).
const CERT_MAX: u64 = 16 * 1024;
/// The keys Georg paired with a yes at the keyboard (`pair_client` under mcp=service), one per line, next to
/// bitsmcp's pairings on the stick (root-only below \BITSOS2\). Only these clients can get full access: a client
/// paired by the token alone (mcp=both, MC-8b's rule) never can.
pub const CONFIRMED_DIR: &str = "/boot/BITSOS2/mcp";
pub const CONFIRMED_NAME: &str = "confirmed";
/// Where a revoke is noted when the data volume did not take it (on the stick): grants set before it are not
/// taken at the next start.
pub const TOMB_NAME: &str = "access-revoked";
/// How often the wall clock's high-water mark is written while a grant is live.
pub const HIGH_WATER_EVERY_S: i64 = 60;
/// How far the wall clock may stand below its high-water mark at a start before the grants end (RTC drift
/// between two writes of the mark, plus a margin).
pub const HIGH_WATER_SLACK_S: i64 = 2 * HIGH_WATER_EVERY_S;

/// A paired client: its key (SHA-256 of the certificate, hex) and its certificate's common name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Client {
    pub fp: String,
    pub name: String,
}

/// The clock: (Unix seconds, monotonic seconds).
pub type Clock = Box<dyn Fn() -> (i64, i64) + Send + Sync>;

pub fn sha256_hex(der: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, der).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// The paired clients in `dir` (`*.der`; a cut `first.bad` and anything not a certificate are left out).
pub fn clients_in(dir: &Path) -> Vec<Client> {
    let mut v: Vec<Client> = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return v };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("der") || e.metadata().map_or(true, |m| !m.is_file() || m.len() > CERT_MAX) {
            continue;
        }
        let Ok(der) = std::fs::read(&p) else { continue };
        let Some(name) = acc::common_name(&der) else { continue };
        let fp = sha256_hex(&der);
        if !v.iter().any(|c| c.fp == fp) {
            v.push(Client { fp, name: if name.is_empty() { String::from("ohne Namen") } else { name } });
        }
    }
    v.sort_by(|a, b| a.fp.cmp(&b.fp));
    v
}

struct Held {
    g: Grant,
    seen: Seen,
}

/// Records a key Georg paired at the keyboard (main: the stick's `confirmed`).
pub type Record = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

pub struct Access {
    held: Mutex<Vec<Held>>,
    /// A save that failed: tried again at every sweep until it is written.
    unsaved: AtomicBool,
    /// The wall time last written as the high-water mark.
    high_water: std::sync::atomic::AtomicI64,
    /// Where a revoke is noted when the volume did not take it (None: tests without it).
    tomb: Option<Box<dyn Keep>>,
    /// A tombstone is written and not yet cleared by a good save.
    tomb_set: AtomicBool,
    record: Option<Record>,
    clients: Mutex<Vec<Client>>,
    keep: Option<Box<dyn Keep>>,
    /// The kernel's table is out of date (main pushes it before the next gate command).
    dirty: AtomicBool,
    clock: Clock,
    /// Reads the paired clients again (a pairing since the start).
    rescan: Box<dyn Fn() -> Vec<Client> + Send + Sync>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn short(fp: &str) -> String {
    acc::fp_short(fp)
}

impl Access {
    /// The grants read at the start (`from_text`), judged from now; the kernel gets them at once (dirty).
    pub fn new(list: Vec<Grant>, keep: Option<Box<dyn Keep>>, clock: Clock, rescan: Box<dyn Fn() -> Vec<Client> + Send + Sync>) -> Access {
        let (wall, mono) = clock();
        let held = list.into_iter().map(|g| Held { g, seen: Seen { wall, mono } }).collect();
        let a = Access {
            held: Mutex::new(held),
            unsaved: AtomicBool::new(false),
            high_water: std::sync::atomic::AtomicI64::new(0),
            tomb: None,
            tomb_set: AtomicBool::new(false),
            record: None,
            clients: Mutex::new(rescan()),
            keep,
            dirty: AtomicBool::new(true),
            clock,
            rescan,
        };
        a.sweep();
        a
    }

    /// Where a revoke the volume did not take is noted (`tomb_set`: one is there from before).
    pub fn with_tombstone(mut self, tomb: Box<dyn Keep>, tomb_set: bool) -> Access {
        self.tomb = Some(tomb);
        self.tomb_set = AtomicBool::new(tomb_set);
        self
    }

    /// How a keyboard-confirmed pairing is recorded.
    pub fn with_pairing_record(mut self, r: Record) -> Access {
        self.record = Some(r);
        self
    }

    /// Georg said yes at the keyboard to pairing `fp` (`pair_client`): from now on that key may get full access.
    pub fn confirm_pairing(&self, fp: &str) {
        if !acc::is_client(fp) {
            log::warn(&format!("the confirmed pairing names no key ({:?}); not recorded", fp.chars().take(80).collect::<String>()));
            return;
        }
        match &self.record {
            Some(r) => match r(fp) {
                Ok(()) => log::info(&format!("pairing {} confirmed at the keyboard: it may get full access", short(fp))),
                Err(e) => log::warn(&format!("pairing {} confirmed at the keyboard, but not recorded ({e}): no full access for it", short(fp))),
            },
            None => {}
        }
    }

    /// The wall clock this store judges by (Unix s).
    pub fn wall_now(&self) -> i64 {
        (self.clock)().0
    }

    /// Whether a save is still owed (the volume did not take the last one).
    pub fn unsaved(&self) -> bool {
        self.unsaved.load(Ordering::Acquire)
    }

    pub fn table(&self) -> Vec<Grant> {
        lock(&self.held).iter().map(|h| h.g.clone()).collect()
    }

    /// True once after every change (the kernel's table must be sent again).
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }
    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    pub fn clients(&self) -> Vec<Client> {
        lock(&self.clients).clone()
    }

    /// The paired client with key `fp`, reading the pairings again when it is not known yet.
    pub fn client(&self, fp: &str) -> Option<Client> {
        if let Some(c) = lock(&self.clients).iter().find(|c| c.fp == fp) {
            return Some(c.clone());
        }
        let fresh = (self.rescan)();
        let found = fresh.iter().find(|c| c.fp == fp).cloned();
        *lock(&self.clients) = fresh;
        found
    }

    /// What the confirmer shows (with the clients' names).
    pub fn shown(&self) -> Vec<Shown> {
        let clients = self.clients();
        lock(&self.held)
            .iter()
            .map(|h| Shown { client: h.g.client.clone(), name: clients.iter().find(|c| c.fp == h.g.client).map_or_else(|| String::from("?"), |c| c.name.clone()), until: h.g.until, nvme: h.g.nvme })
            .collect()
    }

    /// Writes the grants (with the wall clock's high-water mark). When the volume does not take a change that
    /// ends or shortens a grant (`ending`), the revoke is noted on the stick (`tomb`) so the old file cannot bring
    /// it back at the next start, and the save is tried again at every sweep.
    fn save(&self, held: &[Held], ending: bool) {
        let Some(k) = &self.keep else { return };
        let (wall, _) = (self.clock)();
        let hw = wall.max(self.high_water.load(Ordering::Acquire));
        match k.save(&to_text_hw(&held.iter().map(|h| h.g.clone()).collect::<Vec<_>>(), hw)) {
            Ok(()) => {
                self.high_water.store(hw, Ordering::Release);
                if self.unsaved.swap(false, Ordering::AcqRel) {
                    log::info("full-access grants kept on the data volume again");
                }
                if self.tomb_set.load(Ordering::Acquire)
                    && let Some(t) = &self.tomb
                    && t.save("").is_ok()
                {
                    self.tomb_set.store(false, Ordering::Release);
                }
            }
            Err(e) => {
                if !self.unsaved.swap(true, Ordering::AcqRel) {
                    log::warn(&format!("full-access grants not kept on the data volume ({e}); trying again"));
                }
                if ending {
                    match &self.tomb {
                        Some(t) => match t.save(&format!("revoked {wall}\n")) {
                            Ok(()) => {
                                self.tomb_set.store(true, Ordering::Release);
                                log::warn("the end of full access is noted on the stick: the old grants are not taken at the next start");
                            }
                            Err(e2) => log::warn(&format!("the end of full access is not noted anywhere ({e2}): the kernel's table holds it until the next start")),
                        },
                        None => log::warn("the end of full access is not kept: the kernel's table holds it until the next start"),
                    }
                }
            }
        }
    }

    /// Ends what the clock rule ends now (journaled, kept, the kernel told); true when something ended.
    pub fn sweep(&self) -> bool {
        let (wall, mono) = (self.clock)();
        let mut held = lock(&self.held);
        let mut ended: Vec<(Grant, Ended)> = Vec::new();
        held.retain(|h| match acc::judge(&h.g, wall, h.seen, mono) {
            Ok(()) => true,
            Err(why) => {
                ended.push((h.g.clone(), why));
                false
            }
        });
        for (g, why) in &ended {
            log::info(&format!("full access for {} until {} (unix) {}", short(&g.client), g.until, why.text()));
        }
        if !ended.is_empty() {
            self.save(&held, true);
            self.mark_dirty();
        } else if self.unsaved() || (!held.is_empty() && wall.saturating_sub(self.high_water.load(Ordering::Acquire)) >= HIGH_WATER_EVERY_S) {
            // An owed save, or the high-water mark while a grant is live (a clock set back across a start).
            self.save(&held, false);
        }
        !ended.is_empty()
    }

    /// Full access for `client` for `hours` from now, set at the keyboard (only real input; a paired client;
    /// one of the dialog's choices). A grant the client had is replaced.
    pub fn grant(&self, client: &str, hours: i64, nvme: bool, input: Input) -> Result<Grant, String> {
        let refuse = |why: String| {
            log::warn(&format!("full access for {} refused: {why}", short(client)));
            Err(why)
        };
        if input != Input::Physical {
            return refuse(format!("set with {} input; only the real keyboard or mouse sets full access", input.name()));
        }
        if !acc::CHOICES_H.contains(&hours) {
            return refuse(format!("{hours} h is not one of the dialog's choices (1, 8, 24, 168)"));
        }
        let Some(c) = self.client(client) else {
            return refuse(String::from("no paired client has this key"));
        };
        let (wall, mono) = (self.clock)();
        let g = Grant { client: c.fp.clone(), until: wall.saturating_add(hours * 3600), set_at: wall, nvme };
        let mut held = lock(&self.held);
        let mut replaced = false;
        if let Some(i) = held.iter().position(|h| h.g.client == g.client) {
            let o = held.remove(i);
            replaced = o.g.until > g.until || (o.g.nvme && !g.nvme);
            log::info(&format!("full access for {} until {} (unix) {}", short(&o.g.client), o.g.until, Ended::Replaced.text()));
        }
        held.push(Held { g: g.clone(), seen: Seen { wall, mono } });
        log::info(&format!(
            "full access for {} ({}) set at the keyboard for {hours} h, until {} (unix){}",
            short(&c.fp),
            c.name,
            g.until,
            if nvme { ", the NVMe write gate too" } else { "" }
        ));
        self.save(&held, replaced);
        self.mark_dirty();
        Ok(g)
    }

    /// Ends every full-access grant (Georg at the keyboard); how many ended.
    pub fn revoke(&self) -> usize {
        let mut held = lock(&self.held);
        let n = held.len();
        for h in held.iter() {
            log::info(&format!("full access for {} until {} (unix) {}", short(&h.g.client), h.g.until, Ended::Revoked.text()));
        }
        held.clear();
        if n > 0 {
            self.save(&held, true);
            self.mark_dirty();
        }
        n
    }
}

/// The file's form: one line `client until set_at nvme` (nvme 0 or 1) per grant.
pub fn to_text(list: &[Grant]) -> String {
    list.iter().map(|g| format!("{} {} {} {}\n", g.client, g.until, g.set_at, u8::from(g.nvme))).collect()
}

/// The file as written: first `hw <unix s>`, the wall clock's high-water mark, then the grants.
pub fn to_text_hw(list: &[Grant], hw: i64) -> String {
    format!("hw {hw}\n{}", to_text(list))
}

/// The high-water mark in the file's text (None: none, or not one number).
pub fn high_water(text: &str) -> Option<i64> {
    text.lines().find_map(|l| l.strip_prefix("hw ")).and_then(|v| v.trim().parse::<i64>().ok())
}

/// The tombstone's time (`revoked <unix s>`; None: none).
pub fn revoked_at(text: &str) -> Option<i64> {
    text.lines().find_map(|l| l.strip_prefix("revoked ")).and_then(|v| v.trim().parse::<i64>().ok())
}

/// The keys in the stick's `confirmed` (one per line; anything else is skipped).
pub fn confirmed_in(text: &str) -> Vec<String> {
    let mut v: Vec<String> = text.lines().map(str::trim).filter(|l| acc::is_client(l)).map(String::from).collect();
    v.sort();
    v.dedup();
    v
}

/// `confirmed` with `fp` added.
pub fn with_confirmed(text: &str, fp: &str) -> String {
    let mut v = confirmed_in(text);
    if !v.iter().any(|c| c == fp) {
        v.push(String::from(fp));
    }
    v.iter().map(|c| format!("{c}\n")).collect()
}

/// What a start takes from the kept grants: none when the wall clock stands more than `HIGH_WATER_SLACK_S`
/// below the high-water mark written before (the RTC was set back: the grant would grow by the shift), none set
/// at or before a revoke the volume did not take (`revoked`). The clock rule (`Access::new`) judges the rest.
pub fn restore(list: Vec<Grant>, hw: Option<i64>, revoked: Option<i64>, wall: i64) -> Vec<Grant> {
    if let Some(h) = hw
        && wall < h.saturating_sub(HIGH_WATER_SLACK_S)
    {
        for g in &list {
            log::warn(&format!("full access for {} not taken: the clock stands {} s before the last time seen ({h}, unix); it was set back", short(&g.client), h - wall));
        }
        return Vec::new();
    }
    list.into_iter()
        .filter(|g| match revoked {
            Some(r) if g.set_at <= r => {
                log::warn(&format!("full access for {} not taken: revoked at {r} (unix), noted on the stick because the data volume did not take it", short(&g.client)));
                false
            }
            _ => true,
        })
        .collect()
}

/// Reads the file's form; lines that do not parse (or a client named twice) are skipped and counted. The clock
/// rule is applied when the grants are taken (`Access::new` sweeps).
pub fn from_text(text: &str) -> (Vec<Grant>, usize) {
    let mut v: Vec<Grant> = Vec::new();
    let mut bad = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty() && !l.starts_with("hw ")) {
        let w: Vec<&str> = line.split_whitespace().collect();
        let g = match w[..] {
            [c, u, s, n] if acc::is_client(c) && (n == "0" || n == "1") => match (u.parse::<i64>(), s.parse::<i64>()) {
                (Ok(until), Ok(set_at)) if until > set_at && until.checked_sub(set_at).is_some_and(|d| d <= acc::MAX_S) => Some(Grant { client: c.into(), until, set_at, nvme: n == "1" }),
                _ => None,
            },
            _ => None,
        };
        match g {
            Some(g) if !v.iter().any(|o| o.client == g.client) => v.push(g),
            _ => bad += 1,
        }
    }
    (v, bad)
}

#[cfg(test)]
mod tests;
