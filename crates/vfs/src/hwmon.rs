//! SYS-K: the hwmon class (linux-7.3.1/drivers/hwmon/hwmon.c, Documentation/hwmon/sysfs-interface.rst). Drivers in
//! any crate register a chip with its attributes; `/sys/class/hwmon/hwmonN` links to
//! `/sys/devices/virtual/hwmon/hwmonN` with `name`, the attributes, `subsystem` and, when the chip names its
//! device's sysfs directory, `device`. The index is the lowest free one (Linux's `ida_alloc`), so a chip that comes
//! back may get another number: readers find a chip by its `name`, as libsensors and btop do.
//!
//! An attribute's text comes from the driver's closure at each read; it must answer at once (a value the
//! driver's sampler keeps, never device I/O on the reader's thread). An error is the read's answer (Linux's
//! `show` returning -ENODATA and friends).

use crate::Errno;
use crate::inode::Cred;
use crate::synth::{DirSource, FileSource, FlatEntry, FlatKind, FlatView};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::SpinLock;

pub type Read = Arc<dyn Fn() -> Result<String, Errno> + Send + Sync>;

pub struct Attr {
    pub name: String,
    /// 0444, or 0400 for a value only root may read.
    pub mode: u32,
    pub read: Read,
}

impl Attr {
    pub fn new(name: &str, read: impl Fn() -> Result<String, Errno> + Send + Sync + 'static) -> Attr {
        Attr { name: String::from(name), mode: 0o444, read: Arc::new(read) }
    }
    /// A fixed text (a label): `"Tctl"` becomes `"Tctl\n"`.
    pub fn label(name: &str, text: &str) -> Attr {
        let t = format!("{text}\n");
        Attr::new(name, move || Ok(t.clone()))
    }
}

pub struct Chip {
    /// The `name` attribute (`k10temp`, `amdgpu`, `nvme`).
    pub name: String,
    /// The device's directory below /sys (`devices/pci0000:00/0000:c5:00.0`), for the `device` link.
    pub device: Option<String>,
    pub attrs: Vec<Attr>,
}

static CHIPS: SpinLock<Vec<(u32, Arc<Chip>)>> = SpinLock::named("vfs.hwmon", Vec::new());

/// Adds a chip; its index N (`hwmonN`).
pub fn register(chip: Chip) -> u32 {
    let mut l = CHIPS.lock();
    let n = (0..).find(|i| !l.iter().any(|(j, _)| j == i)).unwrap_or(0);
    l.push((n, Arc::new(chip)));
    l.sort_by_key(|(i, _)| *i);
    n
}

/// Takes chip `n` away (its directory and link go); false if unknown.
pub fn unregister(n: u32) -> bool {
    let mut l = CHIPS.lock();
    let before = l.len();
    l.retain(|(i, _)| *i != n);
    l.len() != before
}

/// The chips now, by index.
pub fn chips() -> Vec<(u32, Arc<Chip>)> {
    CHIPS.lock().clone()
}

/// One attribute's file: its text at each read, one page as sysfs shows (`st_size` 4096), no writing.
struct AttrFile(Read);

impl FileSource for AttrFile {
    fn may_open(&self, _who: Cred, write: bool) -> Result<(), Errno> {
        if write { Err(uapi::errno::EACCES) } else { Ok(()) }
    }
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        (self.0)().map(String::into_bytes)
    }
    fn size(&self) -> u64 {
        4096
    }
}

const BASE: &str = "devices/virtual/hwmon";

fn push(out: &mut Vec<FlatEntry>, path: String, kind: FlatKind) {
    out.push(FlatEntry { path, kind });
}

/// The flat list for /sys: both levels of the class and device directories, sorted by path.
pub fn entries_of(chips: &[(u32, Arc<Chip>)]) -> Vec<FlatEntry> {
    let mut out = Vec::new();
    if chips.is_empty() {
        return out;
    }
    for d in ["class", "class/hwmon", "devices", "devices/virtual", BASE] {
        push(&mut out, String::from(d), FlatKind::Dir);
    }
    for (n, c) in chips {
        let dir = format!("{BASE}/hwmon{n}");
        push(&mut out, dir.clone(), FlatKind::Dir);
        push(&mut out, format!("{dir}/name"), FlatKind::Text(format!("{}\n", c.name)));
        for a in &c.attrs {
            push(&mut out, format!("{dir}/{}", a.name), FlatKind::File { perm: a.mode, src: Arc::new(AttrFile(a.read.clone())) });
        }
        if let Some(d) = &c.device {
            push(&mut out, format!("{dir}/device"), FlatKind::Link(format!("../../../../{d}")));
        }
        push(&mut out, format!("{dir}/subsystem"), FlatKind::Link(String::from("../../../../class/hwmon")));
        push(&mut out, format!("class/hwmon/hwmon{n}"), FlatKind::Link(format!("../../{dir}")));
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn view() -> Arc<dyn DirSource> {
    Arc::new(FlatView::new(|| Arc::new(entries_of(&chips()))))
}
crate::procdir!("/sys", view);

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(e: &[FlatEntry]) -> Vec<&str> {
        e.iter().map(|x| x.path.as_str()).collect()
    }

    #[test]
    fn a_chip_has_its_directory_its_link_and_its_attributes() {
        let chip = Chip { name: String::from("k10temp"), device: Some(String::from("devices/pci0000:00/0000:00:18.3")), attrs: alloc::vec![Attr::label("temp1_label", "Tctl"), Attr::new("temp1_input", || Ok(String::from("48125\n")))] };
        let e = entries_of(&[(2, Arc::new(chip))]);
        let p = paths(&e);
        assert!(p.contains(&"class/hwmon/hwmon2") && p.contains(&"devices/virtual/hwmon/hwmon2/temp1_input"));
        assert!(p.windows(2).all(|w| w[0] < w[1]), "sorted, each once");
        let link = |path: &str| e.iter().find(|x| x.path == path).map(|x| match &x.kind { FlatKind::Link(t) => t.clone(), _ => String::new() });
        assert_eq!(link("class/hwmon/hwmon2").as_deref(), Some("../../devices/virtual/hwmon/hwmon2"));
        assert_eq!(link("devices/virtual/hwmon/hwmon2/device").as_deref(), Some("../../../../devices/pci0000:00/0000:00:18.3"));
        let name = e.iter().find(|x| x.path.ends_with("/name")).map(|x| match &x.kind { FlatKind::Text(t) => t.clone(), _ => String::new() });
        assert_eq!(name.as_deref(), Some("k10temp\n"));
        assert!(entries_of(&[]).is_empty(), "no chip: no class directory (as Linux without hwmon drivers)");
    }

    #[test]
    fn indexes_are_the_lowest_free_ones() {
        let mk = || Chip { name: String::from("t"), device: None, attrs: Vec::new() };
        let a = register(mk());
        let b = register(mk());
        assert_ne!(a, b);
        assert!(unregister(a));
        assert_eq!(register(mk()), a, "the freed number comes back first");
        assert!(unregister(a) && unregister(b) && !unregister(b));
    }
}
