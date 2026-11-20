//! PK-0: where `/apps` lives, as one line in `/run/bitsos/apps-state` (init writes it, `bitspkg space` and the
//! Programme page read it). `/apps` is a bind mount of `/data/apps` when the data volume came up writable;
//! otherwise it stays the initramfs tmpfs (gone at the next boot) and the line says why.
//!
//! Lines: `waiting: data volume <phase>` (bitspkg refuses changes until the volume is decided),
//! `data: stick data.img <N> MB`, `data: lexar partition <N> MB`, `tmpfs: <why>`.

pub const APPS: &str = "/apps";
pub const DATA_APPS: &str = "/data/apps";
pub const STATE_FILE: &str = "/run/bitsos/apps-state";
/// The kernel's /data thread (crates/fs-ext4 data.rs `describe`).
pub const DATA_STATE: &str = "/run/bitsos/data-state";
/// NV-2p: whether this boot armed a write window on an NVMe partition (crates/drivers/nvme window.rs `render`).
pub const DATA_WINDOW: &str = "/proc/bitsos/data-window";
/// The stick's data image (data.rs IMAGE) and where the window boot shows it read-only (data.rs STICK_AT).
pub const STICK_IMAGE: &str = "/boot/BITSOS2/data.img";
pub const STICK_DATA: &str = "/stickdata";
/// `blocks=` of data-window counts the disk's logical blocks; user space cannot read their size (change request:
/// `block_size=` in data-window). 512 B is the NVMe default format and the Lexar's. The MB on the line is for
/// people only: a limit (D4, PK-6d) takes the volume's size from statfs, never from this line.
pub const WINDOW_BLOCK: u64 = 512;

/// What /run/bitsos/data-state says about /data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Data {
    /// Not decided yet (the kernel still waits for the stick or the NVMe, or makes the image).
    Pending(String),
    Writable,
    /// /apps cannot go there: the reason for the state line.
    Refused(String),
}

/// `text`: the file's content, None while it does not exist yet.
pub fn data(text: Option<&str>) -> Data {
    let Some(t) = text else { return Data::Pending("not announced yet".into()) };
    let line = t.lines().next().unwrap_or("").trim();
    let mut w = line.split_whitespace();
    match w.next() {
        Some("waiting") => Data::Pending("waiting".into()),
        Some("creating") => Data::Pending("being created".into()),
        Some("mounted") => match (w.next(), w.next()) {
            (Some("pool" | "plain"), Some("rw")) => Data::Writable,
            (Some("pool" | "plain"), Some("ro")) => Data::Refused("data volume read-only".into()),
            _ => Data::Refused(format!("unknown data state {line:?}")),
        },
        Some("skipped") => Data::Refused(format!("no data volume ({})", rest(line, "skipped"))),
        Some("failed") => Data::Refused(format!("data volume failed ({})", rest(line, "failed"))),
        _ => Data::Refused(format!("unknown data state {:?}", clip(line))),
    }
}

fn rest<'a>(line: &'a str, word: &str) -> &'a str {
    let r = line[word.len()..].trim();
    if r.is_empty() { "no reason given" } else { clip(r) }
}

/// A line from a file is shown at most 200 bytes long (on a char boundary).
fn clip(s: &str) -> &str {
    let mut n = s.len().min(200);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

/// Where the writable /data lies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Volume {
    /// NV-2p's window partition (the Lexar on the Mini-PC); MB from data-window's `blocks=`.
    Window { mb: Option<u64> },
    /// The stick's data.img; MB from its size.
    Image { mb: Option<u64> },
    /// Writable, but neither an armed window nor the stick's data.img is seen: nothing to name it by.
    Unknown,
}

/// `Some(blocks)` when data-window says `window=armed` (blocks None when the field is missing or not a number).
pub fn window_armed(text: Option<&str>) -> Option<Option<u64>> {
    let first = text?.lines().next()?;
    let mut armed = false;
    let mut blocks = None;
    for f in first.split_whitespace() {
        match f.split_once('=') {
            Some(("window", "armed")) => armed = true,
            Some(("blocks", n)) => blocks = n.parse::<u64>().ok(),
            _ => {}
        }
    }
    armed.then_some(blocks)
}

/// Which volume /data is, from what user space can see: an armed window carries /data unless the stick's
/// data.img is there and not shown at /stickdata (the window path mounts it there read-only right after /data;
/// a window that armed late or whose pool did not open leaves /data on data.img). None: not sure yet, ask again;
/// `settled` (the wait for /stickdata is over) decides for data.img then.
pub fn volume(window: Option<&str>, image_bytes: Option<u64>, stickdata_mounted: bool, settled: bool) -> Option<Volume> {
    let image = || Volume::Image { mb: image_bytes.map(|b| b >> 20) };
    match window_armed(window) {
        None if image_bytes.is_none() => Some(Volume::Unknown),
        None => Some(image()),
        Some(blocks) if image_bytes.is_none() || stickdata_mounted => {
            Some(Volume::Window { mb: blocks.and_then(|b| b.checked_mul(WINDOW_BLOCK)).map(|b| b >> 20) })
        }
        Some(_) if settled => Some(image()),
        Some(_) => None,
    }
}

pub fn line_data(v: &Volume) -> String {
    let (what, mb) = match v {
        Volume::Window { mb } => ("lexar partition", mb),
        Volume::Image { mb } => ("stick data.img", mb),
        Volume::Unknown => ("unknown volume", &None),
    };
    match mb {
        Some(n) => format!("data: {what} {n} MB\n"),
        None => format!("data: {what}, size unknown\n"),
    }
}

pub fn line_tmpfs(why: &str) -> String {
    format!("tmpfs: {why}\n")
}

pub fn line_waiting(phase: &str) -> String {
    format!("waiting: data volume {phase}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_state_lines() {
        assert_eq!(data(None), Data::Pending("not announced yet".into()));
        assert_eq!(data(Some("waiting\n")), Data::Pending("waiting".into()));
        assert_eq!(data(Some("creating\n")), Data::Pending("being created".into()));
        assert_eq!(data(Some("mounted pool rw\n")), Data::Writable);
        assert_eq!(data(Some("mounted pool rw created\n")), Data::Writable);
        assert_eq!(data(Some("mounted plain rw\n")), Data::Writable);
        assert_eq!(data(Some("mounted pool ro\n")), Data::Refused("data volume read-only".into()));
        assert_eq!(data(Some("skipped data_img_mb=0\n")), Data::Refused("no data volume (data_img_mb=0)".into()));
        assert_eq!(data(Some("skipped\n")), Data::Refused("no data volume (no reason given)".into()));
        assert_eq!(data(Some("failed the image holds no pool\n")), Data::Refused("data volume failed (the image holds no pool)".into()));
        assert!(matches!(data(Some("")), Data::Refused(r) if r.starts_with("unknown data state")));
        assert!(matches!(data(Some("mounted pool\n")), Data::Refused(r) if r.starts_with("unknown data state")));
        let long = format!("{}\n", "ä".repeat(500));
        assert!(matches!(data(Some(&long)), Data::Refused(r) if r.len() < 240));
    }

    #[test]
    fn window_lines() {
        assert_eq!(window_armed(None), None);
        assert_eq!(window_armed(Some("window=none why=no_nvme_data\n")), None);
        assert_eq!(window_armed(Some("window=refused why=no_nvme_write\nCONFIG.TXT has no nvme_write=on\n")), None);
        assert_eq!(window_armed(Some("window=armed disk=nvme0n1 part=nvme0n1p5 first=2048 blocks=1048576000\n")), Some(Some(1048576000)));
        assert_eq!(window_armed(Some("window=armed disk=nvme0n1 part=nvme0n1p5\n")), Some(None));
        assert_eq!(window_armed(Some("window=armed blocks=x\n")), Some(None));
    }

    #[test]
    fn which_volume() {
        let armed = Some("window=armed disk=nvme0n1 part=nvme0n1p5 first=2048 blocks=1048576000 boot_first=1 boot_blocks=2\n");
        let img = Some(2048u64 << 20);
        assert_eq!(volume(None, img, false, false), Some(Volume::Image { mb: Some(2048) }));
        assert_eq!(volume(Some("window=looking\n"), None, false, false), Some(Volume::Unknown));
        assert_eq!(volume(None, None, false, false), Some(Volume::Unknown));
        assert_eq!(volume(armed, None, false, false), Some(Volume::Window { mb: Some(512000) }));
        assert_eq!(volume(armed, img, true, false), Some(Volume::Window { mb: Some(512000) }));
        assert_eq!(volume(armed, img, false, false), None, "data.img there but not at /stickdata: not sure yet");
        assert_eq!(volume(armed, img, false, true), Some(Volume::Image { mb: Some(2048) }));
        assert_eq!(volume(Some("window=armed blocks=18446744073709551615\n"), None, false, false), Some(Volume::Window { mb: None }));
    }

    #[test]
    fn state_lines() {
        assert_eq!(line_data(&Volume::Image { mb: Some(2048) }), "data: stick data.img 2048 MB\n");
        assert_eq!(line_data(&Volume::Window { mb: Some(512000) }), "data: lexar partition 512000 MB\n");
        assert_eq!(line_data(&Volume::Image { mb: None }), "data: stick data.img, size unknown\n");
        assert_eq!(line_data(&Volume::Unknown), "data: unknown volume, size unknown\n");
        assert_eq!(line_tmpfs("no data volume (data_img_mb=0)"), "tmpfs: no data volume (data_img_mb=0)\n");
        assert_eq!(line_waiting("waiting"), "waiting: data volume waiting\n");
    }
}
