//! UPD-4: a version's parts as the update sees them (pure): the bin part from the manifest's `bin` lines, each
//! tree from its `tree` line, which of them the medium holds already (the kernel's `plan` answer), what has to
//! come over the network, and whether it fits on the medium before anything is downloaded.

use crate::manifest::{Entry, Manifest, hex};
use crate::parts_rules as rules;
use mcp_wire::json::Json;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `\BITSOS2\bin`: each asset is one file, named as in `assets`' entry order.
    Bin,
    /// A program tree: its assets are tar + zstd pieces.
    Tree,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    pub name: String,
    pub hash: String,
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub kind: Kind,
    /// Bin: one entry per file (its name is the file's; the asset is `bin_asset(name)`). Tree: its pieces (the
    /// entry's name is the asset's).
    pub assets: Vec<Entry>,
}

impl Part {
    pub fn download_bytes(&self) -> u64 {
        self.assets.iter().map(|e| e.size).fold(0, u64::saturating_add)
    }
}

/// The bin part's content hash: the list of its files as `parts_rules` defines it (names in byte order).
pub fn bin_hash(bins: &[Entry]) -> String {
    let mut h = Sha256::new();
    for e in bins {
        h.update(rules::line(&hex(&e.sha256), e.size, &e.name).as_bytes());
    }
    hex(&h.finalize())
}

/// The parts of a manifest of form 2: bin first, then the trees by name.
pub fn of(m: &Manifest) -> Vec<Part> {
    let mut out = Vec::new();
    if !m.bins.is_empty() {
        out.push(Part {
            name: rules::BIN.to_string(),
            hash: bin_hash(&m.bins),
            files: m.bins.len() as u64,
            dirs: 0,
            bytes: m.bins.iter().map(|e| e.size).fold(0, u64::saturating_add),
            kind: Kind::Bin,
            assets: m.bins.clone(),
        });
    }
    for t in &m.trees {
        out.push(Part { name: t.name.clone(), hash: t.hash.clone(), files: t.files, dirs: t.dirs, bytes: t.bytes, kind: Kind::Tree, assets: t.pieces.clone() });
    }
    out
}

/// What the medium can take (`plan <version>` of the kernel's node).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Room {
    pub free: u64,
    pub cluster: u64,
    /// Freed by the install itself (the slots it prunes and the parts only they name): after the parts are written.
    pub freed: u64,
    /// Parts no slot names: removed before a part would not fit.
    pub orphans: u64,
    /// The sealed parts (name, hash).
    pub stored: Vec<(String, String)>,
    /// A slot of the medium names parts (it runs with them): a release without parts would be a step back.
    pub in_use: bool,
}

pub fn room(j: &Json) -> Result<Room, String> {
    if j.get("ok").and_then(Json::as_bool) != Some(true) {
        return Err(j.get("failed").or_else(|| j.get("refused")).or_else(|| j.get("bad_request")).and_then(Json::as_str).unwrap_or("plan: no answer").to_string());
    }
    let num = |k: &str| j.get(k).and_then(Json::as_u64).unwrap_or(0);
    let stored = j
        .get("parts")
        .and_then(Json::as_array)
        .map(|a| a.iter().filter_map(|p| Some((p.get("name")?.as_str()?.to_string(), p.get("hash")?.as_str()?.to_string()))).collect())
        .unwrap_or_default();
    let in_use = j
        .get("parts")
        .and_then(Json::as_array)
        .is_some_and(|a| a.iter().any(|p| p.get("used_by").and_then(Json::as_array).is_some_and(|u| !u.is_empty())));
    Ok(Room { free: num("free"), cluster: num("cluster"), freed: num("freed"), orphans: num("orphans"), stored, in_use })
}

/// The version's parts against what the medium holds.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub parts: Vec<Part>,
    /// Per part: it is on the medium already (nothing downloaded, nothing written).
    pub present: Vec<bool>,
    pub kernel: u64,
    pub initrd: u64,
}

pub fn plan(m: &Manifest, room: &Room) -> Plan {
    let parts = of(m);
    let present = parts.iter().map(|p| room.stored.iter().any(|(n, h)| *n == p.name && *h == p.hash)).collect();
    let size = |n: &str| m.entry(n).map_or(0, |e| e.size);
    Plan { parts, present, kernel: size("KERNEL"), initrd: size("INITRD") }
}

impl Plan {
    pub fn fetched(&self) -> impl Iterator<Item = &Part> {
        self.parts.iter().zip(&self.present).filter(|(_, p)| !**p).map(|(x, _)| x)
    }
    /// Bytes over the network (and in the work folder until the install): the bundle and every missing part.
    pub fn download(&self) -> u64 {
        self.fetched().map(Part::download_bytes).fold(self.kernel.saturating_add(self.initrd), u64::saturating_add)
    }
    /// The most the missing parts take on the medium.
    pub fn parts_on_medium(&self, cluster: u64) -> u64 {
        self.fetched().map(|p| rules::on_medium(p.files, p.dirs, p.bytes, cluster)).fold(0, u64::saturating_add)
    }
    /// The bundle's own room (KERNEL, INITRD, FILES and the slot's folder, rounded up as install_kernel does).
    pub fn bundle_on_medium(&self, cluster: u64) -> u64 {
        let c = cluster.max(512);
        let up = |b: u64| b.div_ceil(c).saturating_mul(c);
        up(self.kernel).saturating_add(up(self.initrd)).saturating_add(6 * c)
    }
}

/// Whether the version fits (before any download): the parts are written first, into what is free and what no
/// slot uses; the bundle follows after the install's pruning. Err: the bytes still missing.
pub fn fits(p: &Plan, r: &Room) -> Result<(), u64> {
    let parts = p.parts_on_medium(r.cluster);
    let all = parts.saturating_add(p.bundle_on_medium(r.cluster));
    let first = r.free.saturating_add(r.orphans);
    let then = first.saturating_add(r.freed);
    let missing = parts.saturating_sub(first).max(all.saturating_sub(then));
    if missing == 0 { Ok(()) } else { Err(missing) }
}

/// The most the medium needs for this version (for Georg's line).
pub fn need(p: &Plan, r: &Room) -> u64 {
    p.parts_on_medium(r.cluster).saturating_add(p.bundle_on_medium(r.cluster))
}
