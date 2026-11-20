//! Nothing may ever be audible (Georg): the run switches "Master Playback Switch" of every sound card off for its
//! whole length and back to what it was after. The old values are written to `ton.json` before anything is
//! switched, so a run that ends without restoring (a crash, a power cut) is put right at the next start
//! (`recover`). Only the control ioctls CTL_ELEM_READ and CTL_ELEM_WRITE, the element named by interface and
//! name (linux-7.3.1/include/uapi/sound/asound.h `snd_ctl_elem_id`, `snd_ctl_elem_value`; the kernel looks a
//! name up as sound/core/control.c `snd_ctl_find_id` does). No PCM is ever opened here.

use mcpreg::Json;
use std::path::{Path, PathBuf};

pub const SWITCH: &str = "Master Playback Switch";
const IFACE_MIXER: u32 = 2;
const VALUE_SIZE: usize = 1224;
const IFACE: usize = 4;
const NAME: usize = 16;
const NAME_LEN: usize = 44;
const VALUE: usize = 72;

const fn iowr(nr: u32) -> u32 {
    (3 << 30) | ((VALUE_SIZE as u32) << 16) | ((b'U' as u32) << 8) | nr
}
pub const CTL_ELEM_READ: u32 = iowr(0x12);
pub const CTL_ELEM_WRITE: u32 = iowr(0x13);
/// Cards looked at (Linux's SNDRV_CARDS).
pub const CARDS: u32 = 32;

/// One card's control device.
pub trait Ctl {
    fn ioctl(&mut self, req: u32, buf: &mut [u8]) -> Result<(), String>;
}

/// The cards of the machine (`/dev/snd/controlC<n>`), or the tests' fake.
pub trait Mixer {
    fn open(&self, card: u32) -> Option<Box<dyn Ctl>>;
}

pub struct Dev(std::fs::File);

impl Ctl for Dev {
    fn ioctl(&mut self, req: u32, buf: &mut [u8]) -> Result<(), String> {
        use std::os::fd::AsRawFd;
        usys::dev::ioctl_buf(self.0.as_raw_fd() as usize, req, buf).map(|_| ()).map_err(|e| format!("errno {}", e.0))
    }
}

pub struct System;

impl Mixer for System {
    fn open(&self, card: u32) -> Option<Box<dyn Ctl>> {
        let f = std::fs::OpenOptions::new().read(true).write(true).open(format!("/dev/snd/controlC{card}")).ok()?;
        Some(Box::new(Dev(f)))
    }
}

fn id(buf: &mut [u8]) {
    buf[IFACE..IFACE + 4].copy_from_slice(&IFACE_MIXER.to_le_bytes());
    let n = SWITCH.as_bytes();
    buf[NAME..NAME + n.len().min(NAME_LEN - 1)].copy_from_slice(&n[..n.len().min(NAME_LEN - 1)]);
}

/// The switch's two values (left, right) on `c`.
pub fn read(c: &mut dyn Ctl) -> Result<[i64; 2], String> {
    let mut b = vec![0u8; VALUE_SIZE];
    id(&mut b);
    c.ioctl(CTL_ELEM_READ, &mut b)?;
    let v = |i: usize| i64::from_le_bytes(b[VALUE + 8 * i..VALUE + 8 * i + 8].try_into().unwrap_or([0; 8]));
    Ok([v(0), v(1)])
}

pub fn write(c: &mut dyn Ctl, vals: [i64; 2]) -> Result<(), String> {
    let mut b = vec![0u8; VALUE_SIZE];
    id(&mut b);
    for (i, v) in vals.iter().enumerate() {
        b[VALUE + 8 * i..VALUE + 8 * i + 8].copy_from_slice(&v.to_le_bytes());
    }
    c.ioctl(CTL_ELEM_WRITE, &mut b)
}

fn saved_path(root: &Path) -> PathBuf {
    root.join("ton.json")
}

fn saved_json(saved: &[(u32, [i64; 2])]) -> Json {
    let cards = saved.iter().map(|(c, v)| Json::obj(vec![("card", Json::Num(*c as f64)), ("values", Json::Arr(v.iter().map(|x| Json::Num(*x as f64)).collect()))]));
    Json::obj(vec![("cards", Json::Arr(cards.collect()))])
}

fn saved_of(j: &Json) -> Vec<(u32, [i64; 2])> {
    let Some(a) = j.get("cards").and_then(Json::as_array) else { return Vec::new() };
    a.iter()
        .filter_map(|c| {
            let v = c.get("values")?.as_array()?;
            let card = u32::try_from(c.get("card")?.as_u64()?).ok().filter(|n| *n < CARDS)?;
            Some((card, [v.first()?.as_i64()?.clamp(0, 1), v.get(1)?.as_i64()?.clamp(0, 1)]))
        })
        .collect()
}

/// While alive the sound is off; dropping it (any end of the run) switches it back.
pub struct Muted<'a> {
    mixer: &'a dyn Mixer,
    root: PathBuf,
    saved: Vec<(u32, [i64; 2])>,
}

/// Switches every card's master off. Err: no card could be muted (then nothing may run), or the old values could
/// not be kept (then nothing was switched).
pub fn mute<'a>(mixer: &'a dyn Mixer, root: &Path) -> Result<Muted<'a>, String> {
    let mut saved = Vec::new();
    for card in 0..CARDS {
        let Some(mut c) = mixer.open(card) else { continue };
        if let Ok(v) = read(c.as_mut()) {
            saved.push((card, v));
        }
    }
    if saved.is_empty() {
        return Err("keine Tonausgabe gefunden, die sich stumm schalten lässt".into());
    }
    crate::store::write_json(&saved_path(root), &saved_json(&saved))?;
    let m = Muted { mixer, root: root.to_path_buf(), saved };
    for (card, _) in &m.saved {
        let r = m.mixer.open(*card).ok_or_else(|| "gone".to_string()).and_then(|mut c| write(c.as_mut(), [0, 0]));
        if let Err(e) = r {
            // The guard puts back what was switched already.
            return Err(format!("Karte {card} ließ sich nicht stumm schalten ({e})"));
        }
    }
    crate::events::event("sound.muted", &format!("cards={}", m.saved.len()));
    Ok(m)
}

fn put_back(mixer: &dyn Mixer, saved: &[(u32, [i64; 2])]) -> Vec<String> {
    let mut errs = Vec::new();
    for (card, v) in saved {
        if let Err(e) = mixer.open(*card).ok_or_else(|| "gone".to_string()).and_then(|mut c| write(c.as_mut(), *v)) {
            errs.push(format!("card {card}: {e}"));
        }
    }
    errs
}

impl Drop for Muted<'_> {
    fn drop(&mut self) {
        let errs = put_back(self.mixer, &self.saved);
        if errs.is_empty() {
            let _ = std::fs::remove_file(saved_path(&self.root));
            crate::events::event("sound.restored", &format!("cards={}", self.saved.len()));
        } else {
            // ton.json stays: the next start tries again.
            crate::events::event("sound.restore_failed", &format!("why={}", crate::events::q(&errs.join("; "))));
        }
    }
}

/// A `ton.json` left by a run that never restored: its values go back now. Returns the cards restored.
pub fn recover(mixer: &dyn Mixer, root: &Path) -> usize {
    let p = saved_path(root);
    let saved = match crate::store::read_json(&p) {
        Ok(Some(j)) => saved_of(&j),
        _ => return 0,
    };
    let errs = put_back(mixer, &saved);
    if errs.is_empty() {
        let _ = std::fs::remove_file(&p);
        crate::events::event("sound.recovered", &format!("cards={}", saved.len()));
        saved.len()
    } else {
        crate::events::event("sound.restore_failed", &format!("why={}", crate::events::q(&errs.join("; "))));
        0
    }
}

/// `bitsbench ton`: each card's switch, for the checks.
pub fn describe(mixer: &dyn Mixer) -> String {
    let mut out = String::new();
    for card in 0..CARDS {
        let Some(mut c) = mixer.open(card) else { continue };
        match read(c.as_mut()) {
            Ok(v) => out.push_str(&format!("card{card} {SWITCH}={},{}\n", v[0], v[1])),
            Err(e) => out.push_str(&format!("card{card} {SWITCH}: {e}\n")),
        }
    }
    if out.is_empty() { "no sound card\n".into() } else { out }
}

/// The tests' cards: the switch values by card, shared so the test sees what a guard did.
pub mod emu {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Clone, Default)]
    pub struct Cards(pub Rc<RefCell<Vec<Option<[i64; 2]>>>>, pub Rc<RefCell<bool>>);

    struct One(Cards, usize);

    impl Ctl for One {
        fn ioctl(&mut self, req: u32, buf: &mut [u8]) -> Result<(), String> {
            let name = &buf[NAME..NAME + SWITCH.len()];
            if buf.len() != VALUE_SIZE || name != SWITCH.as_bytes() || buf[IFACE] != IFACE_MIXER as u8 {
                return Err("errno 22".into());
            }
            let mut v = self.0.0.borrow_mut();
            let slot = v.get_mut(self.1).and_then(Option::as_mut).ok_or("errno 2")?;
            match req {
                CTL_ELEM_READ => {
                    for (i, x) in slot.iter().enumerate() {
                        buf[VALUE + 8 * i..VALUE + 8 * i + 8].copy_from_slice(&x.to_le_bytes());
                    }
                    Ok(())
                }
                CTL_ELEM_WRITE if *self.0.1.borrow() => Err("errno 5".into()),
                CTL_ELEM_WRITE => {
                    for (i, x) in slot.iter_mut().enumerate() {
                        *x = i64::from_le_bytes(buf[VALUE + 8 * i..VALUE + 8 * i + 8].try_into().unwrap());
                    }
                    Ok(())
                }
                _ => Err("errno 25".into()),
            }
        }
    }

    impl Mixer for Cards {
        fn open(&self, card: u32) -> Option<Box<dyn Ctl>> {
            self.0.borrow().get(card as usize).copied().flatten().map(|_| Box::new(One(self.clone(), card as usize)) as Box<dyn Ctl>)
        }
    }
}
