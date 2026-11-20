//! GM-6c: a small ALSA control client for the sofa mode's volume (plan/spec/sofa-mode.md §7 row "Volume"): the
//! ioctls CTL_ELEM_LIST, CTL_ELEM_INFO, CTL_ELEM_READ and CTL_ELEM_WRITE on `/dev/snd/controlC<n>` as
//! crates/snd/src/ktests_alsa.rs uses them (layouts of linux-7.3.1/include/uapi/sound/asound.h, x86-64), for the
//! element "Master Playback Volume" (integer, two channels, 0..32768 on BitsOS) and the card's "Headphone Jack".
//! Never a PCM: nothing here can make a sound. The ioctls go through the trait `Ctl`, so the host tests answer them
//! with `Emu` (`FileEmu`: kept in a file), a fake that decodes the request structs as the kernel's
//! crates/snd/src/alsa/ctl.rs does (numid or iface+name, the range check's EINVAL). Called from the sofa mode's
//! worker thread only (sofa_io.rs, B4).

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const ELEM_ID_SIZE: usize = 64;
pub const ELEM_LIST_SIZE: usize = 80;
pub const ELEM_INFO_SIZE: usize = 272;
pub const ELEM_VALUE_SIZE: usize = 1224;

const fn iowr(nr: u8, size: usize) -> u64 {
    (3 << 30) | ((size as u64) << 16) | ((b'U' as u64) << 8) | nr as u64
}

pub const CTL_ELEM_LIST: u64 = iowr(0x10, ELEM_LIST_SIZE);
pub const CTL_ELEM_INFO: u64 = iowr(0x11, ELEM_INFO_SIZE);
pub const CTL_ELEM_READ: u64 = iowr(0x12, ELEM_VALUE_SIZE);
pub const CTL_ELEM_WRITE: u64 = iowr(0x13, ELEM_VALUE_SIZE);

// snd_ctl_elem_id
const NUMID: usize = 0;
const IFACE: usize = 4;
const NAME: usize = 16;
const NAME_LEN: usize = 44;
// snd_ctl_elem_list
const LIST_OFFSET: usize = 0;
const LIST_SPACE: usize = 4;
const LIST_USED: usize = 8;
const LIST_COUNT: usize = 12;
const LIST_PIDS: usize = 16;
// snd_ctl_elem_info after the id
const INFO_TYPE: usize = 64;
const INFO_ACCESS: usize = 68;
const INFO_COUNT: usize = 72;
const INFO_MIN: usize = 80;
const INFO_MAX: usize = 88;
// snd_ctl_elem_value: value.integer.value[] (long) after the id and the indirect word
const VALUE: usize = 72;
/// value.integer.value[128]
const VALUES_MAX: usize = 128;

pub const IFACE_CARD: u32 = 0;
pub const IFACE_MIXER: u32 = 2;
pub const TYPE_BOOLEAN: u32 = 1;
pub const TYPE_INTEGER: u32 = 2;
const ACCESS_READ: u32 = 1;
const ACCESS_WRITE: u32 = 2;

pub const MASTER: &str = "Master Playback Volume";
pub const JACK: &str = "Headphone Jack";
/// A card listing more controls than this is cut there (R1: a bound per input; BitsOS's card has five).
pub const LIST_MAX: u32 = 1 << 16;

const EINVAL: i32 = 22;
const ENOENT: i32 = 2;
const EPERM: i32 = 1;

fn get_u32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}
fn put_u32(b: &mut [u8], at: usize, v: u32) {
    if let Some(s) = b.get_mut(at..at + 4) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}
fn get_i64(b: &[u8], at: usize) -> i64 {
    b.get(at..at + 8).map_or(0, |s| i64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
}
fn put_i64(b: &mut [u8], at: usize, v: i64) {
    if let Some(s) = b.get_mut(at..at + 8) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}
fn name_of(b: &[u8]) -> String {
    let n = &b[NAME..NAME + NAME_LEN];
    String::from_utf8_lossy(&n[..n.iter().position(|&c| c == 0).unwrap_or(NAME_LEN)]).into_owned()
}

/// The size an ioctl number says its struct has.
fn size_of_cmd(cmd: u64) -> usize {
    ((cmd >> 16) & 0x3fff) as usize
}

/// One control device. `pids` is the id array an ELEM_LIST fills (the real device gets its address in the struct).
pub trait Ctl {
    fn ioctl(&mut self, cmd: u64, arg: &mut [u8], pids: Option<&mut [u8]>) -> Result<(), i32>;
}

/// A real `/dev/snd/controlC<n>`, opened non-blocking (the control ioctls never wait anyway; reads would).
pub struct Dev(File);

impl Dev {
    pub fn open(p: &Path) -> Result<Dev, String> {
        OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC).open(p).map(Dev).map_err(|e| format!("{}: {e}", p.display()))
    }
}

impl Ctl for Dev {
    fn ioctl(&mut self, cmd: u64, arg: &mut [u8], pids: Option<&mut [u8]>) -> Result<(), i32> {
        if arg.len() != size_of_cmd(cmd) {
            return Err(EINVAL);
        }
        if let Some(p) = pids {
            if (p.len() as u64) < get_u32(arg, LIST_SPACE) as u64 * ELEM_ID_SIZE as u64 {
                return Err(EINVAL);
            }
            let addr = p.as_mut_ptr() as u64;
            arg[LIST_PIDS..LIST_PIDS + 8].copy_from_slice(&addr.to_le_bytes());
        }
        // SAFETY: `arg` is exactly the struct size the ioctl number names (checked above) and lives across the
        // call; an ELEM_LIST's `pids` pointer points at a live buffer of at least `space` ids (checked above).
        // The kernel reads and writes only within those sizes.
        let r = unsafe { libc::ioctl(self.0.as_raw_fd(), cmd as _, arg.as_mut_ptr()) };
        if r < 0 { Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(EINVAL)) } else { Ok(()) }
    }
}

fn errno_text(what: &str, e: i32) -> String {
    format!("{what}: {}", std::io::Error::from_raw_os_error(e))
}

/// A control's id and what ELEM_INFO says of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Info {
    pub numid: u32,
    pub iface: u32,
    pub name: String,
    pub ty: u32,
    pub count: usize,
    pub min: i64,
    pub max: i64,
}

/// Every control's (numid, iface, name): ELEM_LIST once for the count, once for the ids.
pub fn list(c: &mut dyn Ctl) -> Result<Vec<(u32, u32, String)>, String> {
    let mut l = [0u8; ELEM_LIST_SIZE];
    c.ioctl(CTL_ELEM_LIST, &mut l, None).map_err(|e| errno_text("ELEM_LIST", e))?;
    let count = get_u32(&l, LIST_COUNT).min(LIST_MAX);
    let mut l = [0u8; ELEM_LIST_SIZE];
    put_u32(&mut l, LIST_OFFSET, 0);
    put_u32(&mut l, LIST_SPACE, count);
    let mut ids = vec![0u8; count as usize * ELEM_ID_SIZE];
    c.ioctl(CTL_ELEM_LIST, &mut l, Some(&mut ids)).map_err(|e| errno_text("ELEM_LIST", e))?;
    let used = get_u32(&l, LIST_USED).min(count) as usize;
    Ok(ids.chunks(ELEM_ID_SIZE).take(used).map(|id| (get_u32(id, NUMID), get_u32(id, IFACE), name_of(id))).collect())
}

pub fn info(c: &mut dyn Ctl, numid: u32) -> Result<Info, String> {
    let mut b = [0u8; ELEM_INFO_SIZE];
    put_u32(&mut b, NUMID, numid);
    c.ioctl(CTL_ELEM_INFO, &mut b, None).map_err(|e| errno_text("ELEM_INFO", e))?;
    Ok(Info {
        numid: get_u32(&b, NUMID),
        iface: get_u32(&b, IFACE),
        name: name_of(&b),
        ty: get_u32(&b, INFO_TYPE),
        count: (get_u32(&b, INFO_COUNT) as usize).min(VALUES_MAX),
        min: get_i64(&b, INFO_MIN),
        max: get_i64(&b, INFO_MAX),
    })
}

/// The control `name` of interface `iface`, by the card's list.
pub fn find(c: &mut dyn Ctl, iface: u32, name: &str) -> Result<Info, String> {
    let (numid, ..) = list(c)?.into_iter().find(|(_, i, n)| *i == iface && n == name).ok_or_else(|| format!("no control \"{name}\""))?;
    info(c, numid)
}

pub fn read(c: &mut dyn Ctl, i: &Info) -> Result<Vec<i64>, String> {
    let mut b = vec![0u8; ELEM_VALUE_SIZE];
    put_u32(&mut b, NUMID, i.numid);
    c.ioctl(CTL_ELEM_READ, &mut b, None).map_err(|e| errno_text("ELEM_READ", e))?;
    Ok((0..i.count).map(|ch| get_i64(&b, VALUE + ch * 8)).collect())
}

pub fn write(c: &mut dyn Ctl, i: &Info, v: &[i64]) -> Result<(), String> {
    let mut b = vec![0u8; ELEM_VALUE_SIZE];
    put_u32(&mut b, NUMID, i.numid);
    for (ch, x) in v.iter().take(i.count).enumerate() {
        put_i64(&mut b, VALUE + ch * 8, *x);
    }
    c.ioctl(CTL_ELEM_WRITE, &mut b, None).map_err(|e| errno_text("ELEM_WRITE", e))
}

/// A raw value as whole percent of the control's range (rounded).
pub fn pct_of(v: i64, i: &Info) -> u8 {
    let span = (i.max - i.min).max(1) as i128;
    (((v.clamp(i.min, i.max) - i.min) as i128 * 100 + span / 2) / span) as u8
}

/// Percent as a raw value of the control's range.
pub fn raw_of(pct: u8, i: &Info) -> i64 {
    let span = (i.max - i.min).max(0) as i128;
    i.min + ((pct.min(100) as i128 * span + 50) / 100) as i64
}

/// The master volume (the first channel's; the slider sets both alike).
pub fn volume(c: &mut dyn Ctl) -> Result<(Info, u8), String> {
    let i = find(c, IFACE_MIXER, MASTER)?;
    if i.ty != TYPE_INTEGER || i.count == 0 || i.max <= i.min {
        return Err(format!("\"{MASTER}\" is no integer control (type {}, {} values)", i.ty, i.count));
    }
    let v = read(c, &i)?;
    let p = pct_of(v.first().copied().unwrap_or(i.min), &i);
    Ok((i, p))
}

/// Every channel to `pct`; the answer is the value read back.
pub fn set_volume(c: &mut dyn Ctl, pct: u8) -> Result<u8, String> {
    let (i, _) = volume(c)?;
    write(c, &i, &vec![raw_of(pct, &i); i.count])?;
    let v = read(c, &i)?;
    Ok(pct_of(v.first().copied().unwrap_or(i.min), &i))
}

/// Whether headphones are in the jack (the card's "Headphone Jack"); None without that control.
pub fn jack(c: &mut dyn Ctl) -> Option<bool> {
    let i = find(c, IFACE_CARD, JACK).ok()?;
    (i.ty == TYPE_BOOLEAN).then(|| read(c, &i).ok()).flatten().map(|v| v.first().copied().unwrap_or(0) != 0)
}

/// The control devices of `dir` (`controlC<n>`, by n).
pub fn cards(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<(u32, PathBuf)> = std::fs::read_dir(dir)
        .map(|d| d.flatten().filter_map(|e| Some((e.file_name().to_str()?.strip_prefix("controlC")?.parse().ok()?, e.path()))).collect())
        .unwrap_or_default();
    v.sort();
    v.into_iter().map(|(_, p)| p).collect()
}

/// One element of the fake card.
#[derive(Clone, Debug, PartialEq)]
pub struct FakeElem {
    pub iface: u32,
    pub name: String,
    pub ty: u32,
    pub access: u32,
    pub min: i64,
    pub max: i64,
    pub values: Vec<i64>,
}

/// A fake control device answering the four ioctls as the kernel's crates/snd/src/alsa/ctl.rs does.
#[derive(Clone, Debug, PartialEq)]
pub struct Emu {
    pub elems: Vec<FakeElem>,
    /// Every request seen (cmd, numid of the request), for the tests.
    pub seen: Vec<(u64, u32)>,
}

impl Emu {
    /// BitsOS's card: the five controls of ctl.rs, the master at full volume.
    pub fn bitsos() -> Emu {
        let e = |iface, name: &str, ty, access, max, n: usize, v| FakeElem { iface, name: name.into(), ty, access, min: 0, max, values: vec![v; n] };
        let rw = ACCESS_READ | ACCESS_WRITE;
        Emu {
            elems: vec![
                e(IFACE_MIXER, "Master Playback Switch", TYPE_BOOLEAN, rw, 1, 2, 1),
                e(IFACE_MIXER, MASTER, TYPE_INTEGER, rw, 32768, 2, 32768),
                e(IFACE_CARD, JACK, TYPE_BOOLEAN, ACCESS_READ, 1, 1, 0),
                e(IFACE_MIXER, "Capture Switch", TYPE_BOOLEAN, rw, 1, 2, 1),
                e(IFACE_MIXER, "Capture Volume", TYPE_INTEGER, rw, 32768, 2, 32768),
            ],
            seen: Vec::new(),
        }
    }

    fn find(&self, b: &[u8]) -> Option<usize> {
        let numid = get_u32(b, NUMID);
        if numid != 0 {
            return (numid as usize).checked_sub(1).filter(|&i| i < self.elems.len());
        }
        let name = name_of(b);
        self.elems.iter().position(|e| e.iface == get_u32(b, IFACE) && e.name == name)
    }

    fn put_id(&self, b: &mut [u8], i: usize) {
        b[..ELEM_ID_SIZE].fill(0);
        put_u32(b, NUMID, i as u32 + 1);
        put_u32(b, IFACE, self.elems[i].iface);
        let n = self.elems[i].name.as_bytes();
        b[NAME..NAME + n.len().min(NAME_LEN - 1)].copy_from_slice(&n[..n.len().min(NAME_LEN - 1)]);
    }

    /// The master volume's values (the tests' check).
    #[cfg(test)]
    pub fn master(&self) -> Option<&[i64]> {
        self.elems.iter().find(|e| e.name == MASTER).map(|e| e.values.as_slice())
    }
}

impl Ctl for Emu {
    fn ioctl(&mut self, cmd: u64, b: &mut [u8], pids: Option<&mut [u8]>) -> Result<(), i32> {
        if b.len() != size_of_cmd(cmd) {
            return Err(EINVAL);
        }
        self.seen.push((cmd, get_u32(b, NUMID)));
        match cmd {
            CTL_ELEM_LIST => {
                let (mut offset, space) = (get_u32(b, LIST_OFFSET) as usize, get_u32(b, LIST_SPACE));
                put_u32(b, LIST_COUNT, self.elems.len() as u32);
                let mut used = 0u32;
                if space > 0 {
                    let p = pids.ok_or(EINVAL)?;
                    while used < space && offset < self.elems.len() {
                        let at = used as usize * ELEM_ID_SIZE;
                        let slot = p.get_mut(at..at + ELEM_ID_SIZE).ok_or(EINVAL)?;
                        self.put_id(slot, offset);
                        used += 1;
                        offset += 1;
                    }
                }
                put_u32(b, LIST_USED, used);
                Ok(())
            }
            CTL_ELEM_INFO => {
                let i = self.find(b).ok_or(ENOENT)?;
                self.put_id(b, i);
                let e = &self.elems[i];
                put_u32(b, INFO_TYPE, e.ty);
                put_u32(b, INFO_ACCESS, e.access);
                put_u32(b, INFO_COUNT, e.values.len() as u32);
                put_i64(b, INFO_MIN, e.min);
                put_i64(b, INFO_MAX, e.max);
                Ok(())
            }
            CTL_ELEM_READ => {
                let i = self.find(b).ok_or(ENOENT)?;
                if self.elems[i].access & ACCESS_READ == 0 {
                    return Err(EPERM);
                }
                self.put_id(b, i);
                for (c, v) in self.elems[i].values.clone().iter().enumerate() {
                    put_i64(b, VALUE + c * 8, *v);
                }
                Ok(())
            }
            CTL_ELEM_WRITE => {
                let i = self.find(b).ok_or(ENOENT)?;
                let e = &self.elems[i];
                if e.access & ACCESS_WRITE == 0 {
                    return Err(EPERM);
                }
                let v: Vec<i64> = (0..e.values.len()).map(|c| get_i64(b, VALUE + c * 8)).collect();
                if v.iter().any(|x| *x < e.min || *x > e.max) {
                    return Err(EINVAL);
                }
                self.elems[i].values = v;
                Ok(())
            }
            _ => Err(25), // ENOTTY
        }
    }
}

/// A fake card (`BITSCOMP_ALSA_FAKE`, test-only): `Emu` with its values kept in a text file, one line per control
/// "name=v,v" (a missing file or line: BitsOS's defaults), so a test reads what was written.
pub struct FileEmu {
    pub path: PathBuf,
    pub emu: Emu,
}

impl FileEmu {
    pub fn open(path: &Path) -> FileEmu {
        let mut emu = Emu::bitsos();
        let text = std::fs::read_to_string(path).unwrap_or_default();
        for l in text.lines().take(64) {
            let Some((n, v)) = l.split_once('=') else { continue };
            if let Some(e) = emu.elems.iter_mut().find(|e| e.name == n.trim()) {
                let vals: Vec<i64> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
                if vals.len() == e.values.len() {
                    e.values = vals;
                }
            }
        }
        FileEmu { path: path.to_path_buf(), emu }
    }

    fn save(&self) -> std::io::Result<()> {
        let text: String = self.emu.elems.iter().map(|e| format!("{}={}\n", e.name, e.values.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(","))).collect();
        std::fs::write(&self.path, text)
    }
}

impl Ctl for FileEmu {
    fn ioctl(&mut self, cmd: u64, arg: &mut [u8], pids: Option<&mut [u8]>) -> Result<(), i32> {
        self.emu.ioctl(cmd, arg, pids)?;
        if cmd == CTL_ELEM_WRITE {
            self.save().map_err(|e| e.raw_os_error().unwrap_or(5))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ioctl_numbers_are_linuxs() {
        // asound.h: SNDRV_CTL_IOCTL_ELEM_LIST _IOWR('U', 0x10, struct snd_ctl_elem_list) and so on.
        assert_eq!(CTL_ELEM_LIST, 0xc050_5510);
        assert_eq!(CTL_ELEM_INFO, 0xc110_5511);
        assert_eq!(CTL_ELEM_READ, 0xc4c8_5512);
        assert_eq!(CTL_ELEM_WRITE, 0xc4c8_5513);
    }

    #[test]
    fn the_volume_is_found_read_and_written_through_the_four_ioctls() {
        let mut e = Emu::bitsos();
        let names: Vec<String> = list(&mut e).unwrap().into_iter().map(|(_, _, n)| n).collect();
        assert_eq!(names.len(), 5);
        assert_eq!(names[1], MASTER);
        let (i, p) = volume(&mut e).unwrap();
        assert_eq!((i.numid, i.count, i.max, p), (2, 2, 32768, 100));
        assert_eq!(set_volume(&mut e, 30).unwrap(), 30);
        assert_eq!(e.master(), Some(&[9830, 9830][..]));
        let cmds: Vec<u64> = e.seen.iter().map(|(c, _)| *c).collect();
        for c in [CTL_ELEM_LIST, CTL_ELEM_INFO, CTL_ELEM_READ, CTL_ELEM_WRITE] {
            assert!(cmds.contains(&c), "{c:#x} not used");
        }
        assert!(e.seen.iter().any(|(c, n)| *c == CTL_ELEM_WRITE && *n == 2), "written by numid");
    }

    #[test]
    fn out_of_range_and_read_only_writes_are_refused_as_the_kernel_does() {
        let mut e = Emu::bitsos();
        let (i, _) = volume(&mut e).unwrap();
        assert!(write(&mut e, &i, &[40000, 40000]).unwrap_err().contains("ELEM_WRITE"));
        assert_eq!(e.master(), Some(&[32768, 32768][..]), "nothing changed");
        let j = find(&mut e, IFACE_CARD, JACK).unwrap();
        assert!(write(&mut e, &j, &[1]).is_err(), "the jack is read-only");
        assert_eq!(jack(&mut e), Some(false));
        assert!(find(&mut e, IFACE_MIXER, "Nope").is_err());
    }

    #[test]
    fn percent_and_raw_values_round() {
        let i = Info { numid: 2, iface: 2, name: MASTER.into(), ty: 2, count: 2, min: 0, max: 32768 };
        assert_eq!(raw_of(0, &i), 0);
        assert_eq!(raw_of(100, &i), 32768);
        assert_eq!(raw_of(255, &i), 32768);
        for p in 0..=100u8 {
            assert_eq!(pct_of(raw_of(p, &i), &i), p);
        }
        assert_eq!(pct_of(-5, &i), 0);
        let odd = Info { min: -10, max: 10, ..i };
        assert_eq!(pct_of(0, &odd), 50);
    }

    #[test]
    fn a_card_without_a_master_volume_says_so() {
        let mut e = Emu { elems: Emu::bitsos().elems.into_iter().filter(|x| x.name != MASTER).collect(), seen: Vec::new() };
        assert!(volume(&mut e).unwrap_err().contains("no control"));
        assert!(set_volume(&mut e, 50).is_err());
    }

    #[test]
    fn the_file_fake_keeps_what_was_written() {
        let d = std::env::temp_dir().join(format!("bitscomp-alsa-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("ctl.txt");
        std::fs::write(&p, "Master Playback Volume=16384,16384\nbad line\n").unwrap();
        let mut f = FileEmu::open(&p);
        assert_eq!(volume(&mut f).unwrap().1, 50);
        assert_eq!(set_volume(&mut f, 55).unwrap(), 55);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("Master Playback Volume=18022,18022\n"), "{text}");
        assert_eq!(volume(&mut FileEmu::open(&p)).unwrap().1, 55);
        std::fs::write(d.join("controlC1"), "").unwrap();
        std::fs::write(d.join("controlC0"), "").unwrap();
        std::fs::write(d.join("pcmC0D0p"), "").unwrap();
        assert_eq!(cards(&d), vec![d.join("controlC0"), d.join("controlC1")]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_struct_of_the_wrong_size_is_never_sent() {
        let mut e = Emu::bitsos();
        let mut short = [0u8; 10];
        assert_eq!(e.ioctl(CTL_ELEM_INFO, &mut short, None), Err(EINVAL));
    }
}
