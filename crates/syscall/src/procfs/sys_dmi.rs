//! SYS-K: `/sys/class/dmi/id` -> `/sys/devices/virtual/dmi/id` (linux-7.3.1/drivers/firmware/dmi-id.c): one file per
//! identity string the firmware's SMBIOS table has (`acpi::dmi`, read once at boot), `%s\n`; serial numbers and
//! the UUID 0400 (root), the rest 0444. Without an SMBIOS table there is no `dmi` directory, as on Linux.

use super::attr::Live;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use vfs::synth::{DirSource, FlatEntry, FlatKind, FlatView};

const DIR: &str = "devices/virtual/dmi/id";

fn entries() -> Vec<FlatEntry> {
    let f = acpi::dmi::fields();
    if f.is_empty() {
        return Vec::new();
    }
    let dir = |p: &str| FlatEntry { path: String::from(p), kind: FlatKind::Dir };
    let mut v = alloc::vec![
        dir("class"),
        dir("class/dmi"),
        FlatEntry { path: String::from("class/dmi/id"), kind: FlatKind::Link(String::from("../../devices/virtual/dmi/id")) },
        dir("devices"),
        dir("devices/virtual"),
        dir("devices/virtual/dmi"),
        dir(DIR),
    ];
    for x in f {
        let text = format!("{}\n", x.text);
        let kind = if x.mode == 0o444 { FlatKind::Text(text) } else { FlatKind::File { perm: x.mode, src: Arc::new(Live::text(text)) } };
        v.push(FlatEntry { path: format!("{DIR}/{}", x.name), kind });
    }
    v.sort_by(|a, b| a.path.cmp(&b.path));
    v
}

fn view() -> Arc<dyn DirSource> {
    Arc::new(FlatView::new(|| Arc::new(entries())))
}
vfs::procdir!("/sys", view);
