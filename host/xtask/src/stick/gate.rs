//! Which mounted file system is Georg's stick. Every check runs before a plan is shown and again after the J,
//! so a stick swapped while the question waited is refused. Nothing here writes; device facts come from the
//! mount table and the udev database (`lsblk`), never from reading a block device.
use std::path::{Path, PathBuf};
use std::process::Command;

pub const LABEL: &str = "BITSOS";
/// V2's loader names itself so in its log; V1's loader says "BitsOS loader".
pub const V2_LOADER_MARK: &[u8] = b"BitsOS V2 loader";
const V1_LOADER_MARK: &[u8] = b"BitsOS loader";
/// Mount points of the running system: a stick mounted there, or sharing a disk with them, is never written.
const SYSTEM_POINTS: &[&str] = &["/", "/boot", "/boot/efi", "/efi", "/home", "/usr", "/var"];
/// Buses of built-in disks: refused even with --force (R8b).
const FIXED_BUSES: &[&str] = &["nvme", "sata", "ata", "sas", "fc", "iscsi", "pcie", "virtio", "ide"];
/// Bound against a hostile file standing in for STICK.ID or the loader (they are 34 bytes and ~200 KB).
const MAX_SMALL: u64 = 4096;
const MAX_LOADER: u64 = 64 << 20;

pub struct Mount {
    pub dev: String,
    pub point: String,
    pub fstype: String,
    pub opts: String,
}

/// /proc/mounts escapes space, tab, newline and backslash as octal.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            let v = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 4]).unwrap_or("077"), 8).unwrap_or(b'?');
            out.push(v);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

pub fn parse_mounts(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some(Mount { dev: unescape(f.next()?), point: unescape(f.next()?), fstype: f.next()?.to_string(), opts: f.next().unwrap_or("").to_string() })
        })
        .collect()
}

pub fn find_mount<'a>(ms: &'a [Mount], target: &Path) -> Option<&'a Mount> {
    // The last entry for a mount point wins (stacked mounts).
    ms.iter().rev().find(|m| Path::new(&m.point) == target)
}

/// What udev knows about the mounted device and the disk it sits on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Blk {
    pub label: String,
    pub fstype: String,
    pub fsver: String,
    pub uuid: String,
    pub size: String,
    /// The whole disk under the device (through partitions, dm-crypt, LVM: `lsblk -s`); `disk_kind`, `tran`
    /// and `model` are that disk's.
    pub disk: String,
    /// Every whole disk (or loop device) the device sits on; more than one for RAID or LVM over disks.
    pub disks: Vec<String>,
    pub disk_kind: String,
    pub tran: String,
    pub model: String,
    /// The image file behind a loop device.
    pub backing: String,
}

/// The host facts the gate reads; tests put a fake in.
pub trait Probe {
    fn mounts(&self) -> Result<String, String>;
    fn blk(&self, dev: &str) -> Result<Blk, String>;
    fn free_bytes(&self, p: &Path) -> Result<u64, String>;
}

pub struct HostProbe;

/// `KEY="value" KEY2="..."` lines of `lsblk -P`; values escape odd bytes as `\xHH`.
pub fn parse_pairs(line: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && b[i] == b' ' {
            i += 1;
        }
        let k0 = i;
        while i < b.len() && b[i] != b'=' {
            i += 1;
        }
        if i + 1 >= b.len() || b[i + 1] != b'"' {
            break;
        }
        let key = String::from_utf8_lossy(&b[k0..i]).to_string();
        i += 2;
        let mut v = Vec::new();
        while i < b.len() && b[i] != b'"' {
            if b[i] == b'\\'
                && i + 3 < b.len()
                && b[i + 1] == b'x'
                && let Ok(x) = u8::from_str_radix(std::str::from_utf8(&b[i + 2..i + 4]).unwrap_or("zz"), 16)
            {
                v.push(x);
                i += 4;
                continue;
            }
            v.push(b[i]);
            i += 1;
        }
        i += 1;
        out.push((key, String::from_utf8_lossy(&v).to_string()));
    }
    out
}

fn get<'a>(p: &'a [(String, String)], k: &str) -> &'a str {
    p.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str()).unwrap_or("")
}

fn lsblk(args: &[&str], dev: &str) -> Result<Vec<Vec<(String, String)>>, String> {
    let o = Command::new("lsblk").args(["-P", "-p", "-n"]).args(args).arg(dev).output().map_err(|e| format!("lsblk: {e}"))?;
    if !o.status.success() {
        return Err(format!("lsblk {dev}: {}", String::from_utf8_lossy(&o.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&o.stdout).lines().map(parse_pairs).collect())
}

impl Probe for HostProbe {
    fn mounts(&self) -> Result<String, String> {
        std::fs::read_to_string("/proc/self/mounts").map_err(|e| format!("/proc/self/mounts: {e}"))
    }

    fn blk(&self, dev: &str) -> Result<Blk, String> {
        // -s lists the device first, then everything it sits on down to the whole disks.
        let rows = lsblk(&["-s", "-o", "NAME,TYPE,LABEL,FSTYPE,FSVER,UUID,SIZE,TRAN,MODEL"], dev)?;
        let me = rows.iter().find(|r| get(r, "NAME") == dev).or(rows.first()).ok_or_else(|| format!("lsblk knows no {dev}"))?;
        let mut disks: Vec<&Vec<(String, String)>> = Vec::new();
        for r in rows.iter().filter(|r| matches!(get(r, "TYPE"), "disk" | "loop")) {
            if !disks.iter().any(|d| get(d, "NAME") == get(r, "NAME")) {
                disks.push(r);
            }
        }
        let d = disks.first().ok_or_else(|| format!("lsblk names no whole disk under {dev}"))?;
        let mut b = Blk {
            label: get(me, "LABEL").into(),
            fstype: get(me, "FSTYPE").into(),
            fsver: get(me, "FSVER").into(),
            uuid: get(me, "UUID").into(),
            size: get(me, "SIZE").into(),
            disk: get(d, "NAME").into(),
            disks: disks.iter().map(|d| get(d, "NAME").to_string()).collect(),
            disk_kind: get(d, "TYPE").into(),
            tran: get(d, "TRAN").into(),
            model: get(d, "MODEL").trim().into(),
            backing: String::new(),
        };
        if b.disk_kind == "loop" {
            let n = Path::new(&b.disk).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            b.backing = std::fs::read_to_string(format!("/sys/block/{n}/loop/backing_file")).unwrap_or_default().trim().into();
        }
        Ok(b)
    }

    fn free_bytes(&self, p: &Path) -> Result<u64, String> {
        let o = Command::new("stat").args(["-f", "-c", "%a %S"]).arg(p).output().map_err(|e| format!("stat -f: {e}"))?;
        let t = String::from_utf8_lossy(&o.stdout);
        let mut f = t.split_whitespace().map(|x| x.parse::<u64>());
        match (f.next(), f.next()) {
            (Some(Ok(a)), Some(Ok(s))) if o.status.success() => Ok(a.saturating_mul(s)),
            _ => Err(format!("stat -f {}: {}", p.display(), String::from_utf8_lossy(&o.stderr).trim())),
        }
    }
}

/// The entry of `dir` whose name equals `name` ignoring ASCII case (FAT is case-insensitive; this also works
/// on the host test directories).
pub fn find_ci(dir: &Path, name: &str) -> Option<PathBuf> {
    let rd = std::fs::read_dir(dir).ok()?;
    rd.flatten().find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(name)).map(|e| e.path())
}

/// `rel` (`/`-separated) below `root`, every component matched ignoring case; `None` if a part is missing.
pub fn resolve_ci(root: &Path, rel: &str) -> Option<PathBuf> {
    rel.split('/').filter(|c| !c.is_empty()).try_fold(root.to_path_buf(), |p, c| find_ci(&p, c))
}

/// Reads a file of at most `max` bytes (a larger one is refused, never read into memory).
pub fn read_bounded(p: &Path, max: u64) -> Result<Vec<u8>, String> {
    let m = std::fs::metadata(p).map_err(|e| format!("{}: {e}", p.display()))?;
    if !m.is_file() {
        return Err(format!("{} is not a file", p.display()));
    }
    if m.len() > max {
        return Err(format!("{} has {} bytes, more than the {max} allowed", p.display(), m.len()));
    }
    std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))
}

/// STICK.ID: 32 hex digits, white space around them allowed (bitsboot `volume::parse_stick_id`).
pub fn parse_id(text: &[u8]) -> Option<[u8; 16]> {
    let t = std::str::from_utf8(text).ok()?.trim();
    if t.len() != 32 || !t.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut id = [0u8; 16];
    for (i, b) in id.iter_mut().enumerate() {
        *b = u8::from_str_radix(&t[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(id)
}

pub fn id_hex(id: &[u8; 16]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loader {
    None,
    V1,
    V2,
    Foreign,
}

/// Whose `\EFI\BOOT\BOOTX64.EFI` this is. V1's rule (make-stick.ps1): a loader next to `\BITSOS\KERNEL.BIN`
/// is BitsOS's.
pub fn classify_loader(bytes: Option<&[u8]>, v1_kernel: bool) -> Loader {
    match bytes {
        None => Loader::None,
        Some(b) if contains(b, V2_LOADER_MARK) => Loader::V2,
        Some(b) if contains(b, V1_LOADER_MARK) || v1_kernel => Loader::V1,
        Some(_) => Loader::Foreign,
    }
}

/// The stick as the gate found it.
#[derive(Clone, Debug)]
pub struct Ident {
    pub point: PathBuf,
    pub dev: String,
    pub blk: Blk,
    pub read_only: bool,
    pub id: [u8; 16],
    /// The STICK.ID file exactly as found (moved byte for byte by --retire-v1).
    pub id_bytes: Vec<u8>,
    /// The identity was read from `\BITSOS\STICK.ID` (no `\BITSOS2\STICK.ID` yet).
    pub id_from_v1: bool,
    /// `\BITSOS\` exists: V1 is still on the stick.
    pub v1_dir: bool,
    pub loader: Loader,
    pub loader_bytes: Option<Vec<u8>>,
    /// The device is not on USB (and not a loop image), accepted with --force.
    pub forced_bus: Option<String>,
}

impl Ident {
    /// What must not change between the plan and the write.
    pub fn same_stick(&self, o: &Ident) -> bool {
        self.point == o.point && self.dev == o.dev && self.blk.uuid == o.blk.uuid && self.blk.label == o.blk.label && self.id == o.id
    }

    pub fn describe(&self) -> String {
        let bus = if self.blk.disk_kind == "loop" {
            format!("loop image {}", if self.blk.backing.is_empty() { "?" } else { &self.blk.backing })
        } else {
            format!("{} {}", if self.blk.tran.is_empty() { "unknown bus" } else { &self.blk.tran }, self.blk.model)
        };
        format!(
            "{} = {} ({}), label {}, {}, UUID {}, {}{}",
            self.point.display(),
            self.dev,
            bus.trim(),
            self.blk.label,
            self.blk.fsver,
            self.blk.uuid,
            self.blk.size,
            if self.read_only { ", mounted READ-ONLY" } else { "" }
        )
    }
}

fn refuse(why: String) -> String {
    format!("refused: {why}. Nothing was written.")
}

/// The whole disks under every system mount (/, /boot, ...) that sits on a block device. Fails closed: a
/// system mount whose disks cannot be told refuses the stick (R8b), it never drops out of the check.
fn system_disks(p: &dyn Probe, ms: &[Mount]) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for m in ms.iter().filter(|m| SYSTEM_POINTS.contains(&m.point.as_str()) && m.dev.starts_with("/dev/")) {
        let b = p.blk(&m.dev).map_err(|e| refuse(format!("cannot tell which disk holds {} of the running system ({e})", m.point)))?;
        if b.disks.is_empty() {
            return Err(refuse(format!("cannot tell which disk holds {} of the running system ({})", m.point, m.dev)));
        }
        out.extend(b.disks.into_iter().map(|d| (m.point.clone(), d)));
    }
    Ok(out)
}

/// All checks. `recorded` = the stick ID recorded on the Gaming-PC; `None` refuses at the identity step.
/// `force` only lets a stick on another bus than USB through; never the label, the identity, a built-in disk
/// or a foreign loader.
pub fn identify(p: &dyn Probe, target: &Path, recorded: Option<[u8; 16]>, force: bool) -> Result<Ident, String> {
    let point = target.canonicalize().map_err(|e| refuse(format!("{}: {e}", target.display())))?;
    let ms = parse_mounts(&p.mounts()?);
    let m = find_mount(&ms, &point).ok_or_else(|| refuse(format!("{} is not a mount point (give the stick's mount point)", point.display())))?;
    if SYSTEM_POINTS.iter().any(|s| Path::new(s) == point) {
        return Err(refuse(format!("{} is a mount point of the running system", point.display())));
    }
    if m.fstype != "vfat" {
        return Err(refuse(format!("{} is {} on {}, not FAT32 (vfat)", point.display(), m.fstype, m.dev)));
    }
    if !m.dev.starts_with("/dev/") {
        return Err(refuse(format!("{} comes from {}, not from a block device", point.display(), m.dev)));
    }
    let blk = p.blk(&m.dev).map_err(|e| refuse(format!("cannot read the label of {}: {e}", m.dev)))?;
    if blk.label != LABEL {
        let l = if blk.label.is_empty() { "no label".to_string() } else { format!("label '{}'", blk.label) };
        return Err(refuse(format!("{} has {l}, the stick must be labelled {LABEL} (--force never changes this)", m.dev)));
    }
    if blk.fsver != "FAT32" {
        return Err(refuse(format!("{} is {} {}, not FAT32", m.dev, blk.fstype, if blk.fsver.is_empty() { "of unknown version" } else { &blk.fsver })));
    }
    if blk.disks.len() != 1 || blk.disks[0] != blk.disk {
        return Err(refuse(format!("{} does not sit on exactly one disk ({:?}); a stick is one USB disk", m.dev, blk.disks)));
    }
    let mut forced_bus = None;
    if blk.disk_kind != "loop" && blk.tran != "usb" {
        let bus = if blk.tran.is_empty() { "an unknown bus".to_string() } else { blk.tran.clone() };
        if FIXED_BUSES.contains(&blk.tran.as_str()) {
            return Err(refuse(format!("{} is not on USB but on {bus} (a built-in disk; --force never changes this)", m.dev)));
        }
        if !force {
            return Err(refuse(format!("{} is not on USB but on {bus} (--force accepts it if you are sure)", m.dev)));
        }
        forced_bus = Some(bus);
    }
    if let Some((sp, _)) = system_disks(p, &ms)?.into_iter().find(|(_, d)| *d == blk.disk) {
        return Err(refuse(format!("{} is on {}, the disk that also holds {sp} of the running system", m.dev, blk.disk)));
    }
    if find_ci(&point, "Windows").map(|w| w.is_dir()).unwrap_or(false) {
        return Err(refuse(format!("{} has a Windows folder", point.display())));
    }
    let read_only = m.opts.split(',').any(|o| o == "ro");

    type Found = Option<(Vec<u8>, [u8; 16])>;
    let read_id = |rel: &str| -> Result<Found, String> {
        let Some(f) = resolve_ci(&point, rel) else { return Ok(None) };
        let bytes = read_bounded(&f, MAX_SMALL).map_err(|e| refuse(format!("\\{}: {e}", rel.replace('/', "\\"))))?;
        let id = parse_id(&bytes).ok_or_else(|| refuse(format!("\\{} holds no stick ID (32 hex digits)", rel.replace('/', "\\"))))?;
        Ok(Some((bytes, id)))
    };
    let v2 = read_id("BITSOS2/STICK.ID")?;
    let v1 = read_id("BITSOS/STICK.ID")?;
    if let (Some((_, a)), Some((_, b))) = (&v2, &v1)
        && a != b
    {
        return Err(refuse(format!("\\BITSOS2\\STICK.ID ({}) and \\BITSOS\\STICK.ID ({}) differ", id_hex(a), id_hex(b))));
    }
    let id_from_v1 = v2.is_none() && v1.is_some();
    let (id_bytes, id) = v2.or(v1).ok_or_else(|| refuse("neither \\BITSOS2\\STICK.ID nor \\BITSOS\\STICK.ID exists: not Georg's BitsOS stick".into()))?;
    match recorded {
        None => return Err(refuse(format!("no recorded stick ID to compare with (this stick says {})", id_hex(&id)))),
        Some(r) if r != id => return Err(refuse(format!("the stick's ID {} is not the recorded ID {}", id_hex(&id), id_hex(&r)))),
        Some(_) => {}
    }

    let v1_dir = find_ci(&point, "BITSOS").map(|d| d.is_dir()).unwrap_or(false);
    let v1_kernel = resolve_ci(&point, "BITSOS/KERNEL.BIN").is_some();
    let loader_bytes = match resolve_ci(&point, "EFI/BOOT/BOOTX64.EFI") {
        Some(f) => Some(read_bounded(&f, MAX_LOADER).map_err(|e| refuse(format!("\\EFI\\BOOT\\BOOTX64.EFI: {e}")))?),
        None => None,
    };
    let loader = classify_loader(loader_bytes.as_deref(), v1_kernel);
    if loader == Loader::Foreign {
        return Err(refuse("\\EFI\\BOOT\\BOOTX64.EFI belongs to another system (neither V1's nor V2's loader)".into()));
    }
    Ok(Ident { point, dev: m.dev.clone(), blk, read_only, id, id_bytes, id_from_v1, v1_dir, loader, loader_bytes, forced_bus })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTS: &str = "/dev/nvme0n1p2 / ext4 rw 0 0\n/dev/sdb1 /run/media/georg/BITS\\040OS vfat rw,nosuid 0 0\ntmpfs /tmp tmpfs rw 0 0\n";

    #[test]
    fn mounts_are_parsed_and_unescaped() {
        let ms = parse_mounts(MOUNTS);
        assert_eq!(ms.len(), 3);
        assert_eq!(ms[1].point, "/run/media/georg/BITS OS");
        assert_eq!(ms[1].opts, "rw,nosuid");
        let m = find_mount(&ms, Path::new("/run/media/georg/BITS OS")).unwrap();
        assert_eq!((m.dev.as_str(), m.fstype.as_str()), ("/dev/sdb1", "vfat"));
        assert!(find_mount(&ms, Path::new("/run/media/georg")).is_none());
        // A trailing lone backslash and a short escape do not panic.
        assert_eq!(parse_mounts("a\\ b\\04 c d\n")[0].dev, "a\\");
    }

    #[test]
    fn lsblk_pairs() {
        let p = parse_pairs(r#"NAME="/dev/sdb1" LABEL="MY\x20STICK" FSVER="FAT32" MODEL="" X="a\"#);
        assert_eq!(get(&p, "NAME"), "/dev/sdb1");
        assert_eq!(get(&p, "LABEL"), "MY STICK");
        assert_eq!(get(&p, "FSVER"), "FAT32");
        assert_eq!(get(&p, "MODEL"), "");
        assert!(parse_pairs("garbage").is_empty());
    }

    #[test]
    fn ids() {
        let id = parse_id(b"a9d836bfd4109c0203b9030cb30a8463\r\n").unwrap();
        assert_eq!(id_hex(&id), "a9d836bfd4109c0203b9030cb30a8463");
        assert_eq!(parse_id(b"A9D836BFD4109C0203B9030CB30A8463"), Some(id));
        for bad in [&b""[..], b"a9d836bf", b"a9d836bfd4109c0203b9030cb30a846g", b"a9d836bfd4109c0203b9030cb30a84631", b"\xff\xfe"] {
            assert!(parse_id(bad).is_none());
        }
    }

    #[test]
    fn loaders() {
        assert_eq!(classify_loader(None, true), Loader::None);
        assert_eq!(classify_loader(Some(b"MZ..bitsboot: BitsOS V2 loader (BT-1)"), true), Loader::V2);
        assert_eq!(classify_loader(Some(b"MZ..BitsOS loader on"), false), Loader::V1);
        assert_eq!(classify_loader(Some(b"MZ..grub"), true), Loader::V1);
        assert_eq!(classify_loader(Some(b"MZ..grub"), false), Loader::Foreign);
    }
}
