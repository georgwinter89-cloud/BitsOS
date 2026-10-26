//! `/dev/snd/controlC0` (`linux-6.6/sound/core/control.c`, the PCM part of `pcm.c:83`): card info, five
//! elements (master volume and switch on the mixer, the headphone jack read from `hda`, SN-9's capture switch
//! and volume applied by `capture` as frames are read), value-change events, the PCM device queries libasound
//! makes when it opens `hw:0,0`.

use super::abi::*;
use crate::client::UNITY;
use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use hdacodec::path::OutputKind;
use ksync::{SpinLock, WaitQueue};
use uapi::errno::*;
use vfs::node::File;
use vfs::poll::{Events, PollQueue};
use vfs::sys::UserMem;
use vfs::Errno;

const IFACE_CARD: u32 = 0;
const IFACE_MIXER: u32 = 2;
const TYPE_BOOLEAN: u32 = 1;
const TYPE_INTEGER: u32 = 2;
const ACCESS_READ: u32 = 1;
const ACCESS_WRITE: u32 = 2;
const EVENT_MASK_VALUE: u32 = 1;

struct Elem {
    iface: u32,
    name: &'static str,
    ty: u32,
    access: u32,
    count: usize,
    min: i64,
    max: i64,
}

/// numid = index + 1 (Linux numbers the controls in the order they are added).
const ELEMS: [Elem; 5] = [
    Elem { iface: IFACE_MIXER, name: "Master Playback Switch", ty: TYPE_BOOLEAN, access: ACCESS_READ | ACCESS_WRITE, count: 2, min: 0, max: 1 },
    Elem { iface: IFACE_MIXER, name: "Master Playback Volume", ty: TYPE_INTEGER, access: ACCESS_READ | ACCESS_WRITE, count: 2, min: 0, max: UNITY as i64 },
    Elem { iface: IFACE_CARD, name: "Headphone Jack", ty: TYPE_BOOLEAN, access: ACCESS_READ, count: 1, min: 0, max: 1 },
    // SN-9, appended so the numids above stay (hda_generic.c names them so for the ADC's amp).
    Elem { iface: IFACE_MIXER, name: "Capture Switch", ty: TYPE_BOOLEAN, access: ACCESS_READ | ACCESS_WRITE, count: 2, min: 0, max: 1 },
    Elem { iface: IFACE_MIXER, name: "Capture Volume", ty: TYPE_INTEGER, access: ACCESS_READ | ACCESS_WRITE, count: 2, min: 0, max: UNITY as i64 },
];
const CAPTURE_SWITCH: usize = 3;
const CAPTURE_VOLUME: usize = 4;

/// Master switch and volume (Q15 per channel): kept here, applied to the kernel mixer as switch * volume.
static MASTER: SpinLock<([bool; 2], [u32; 2])> = SpinLock::named("snd.ctl.master", ([true; 2], [UNITY; 2]));
/// Writers of the capture controls one at a time (the switch and volume are stored as a pair).
static CAPTURE: SpinLock<()> = SpinLock::named("snd.ctl.capture", ());
/// The open control files that subscribed to events.
static SUBSCRIBED: SpinLock<Vec<Weak<Events_>>> = SpinLock::named("snd.ctl.subs", Vec::new());

/// One open file's event queue (`snd_ctl_file.events`).
struct Events_ {
    q: SpinLock<VecDeque<(u32, u32)>>,
    poll: PollQueue,
    /// Blocking readers.
    wq: WaitQueue,
}

pub struct Ctl {
    open: bool,
    ev: Arc<Events_>,
    subscribed: SpinLock<bool>,
    prefer_subdevice: SpinLock<i32>,
    /// Calls per ioctl number ('U'), logged at close (the trace of what libasound asks a card).
    calls: [AtomicU32; 256],
}

fn headphone_plugged() -> bool {
    hda::controllers().iter().filter(|d| !d.ctrl.kind().enumerate_only()).flat_map(|d| d.ctrl.outputs()).any(|o| o.kind == OutputKind::Headphone && o.plugged == Some(true))
}

fn values(i: usize) -> Vec<i64> {
    let (sw, vol) = *MASTER.lock();
    let (csw, cvol) = crate::capture::gain();
    match i {
        0 => sw.iter().map(|&b| b as i64).collect(),
        1 => vol.iter().map(|&v| v as i64).collect(),
        CAPTURE_SWITCH => csw.iter().map(|&b| b as i64).collect(),
        CAPTURE_VOLUME => cvol.iter().map(|&v| v as i64).collect(),
        _ => alloc::vec![headphone_plugged() as i64],
    }
}

/// The capture controls: stored in `capture` (atomics read per read call), no mixer involved.
fn capture_write(i: usize, v: &[i64]) -> bool {
    let _g = CAPTURE.lock();
    let (mut sw, mut vol) = crate::capture::gain();
    let old = (sw, vol);
    for c in 0..2 {
        if i == CAPTURE_SWITCH {
            sw[c] = v[c] != 0;
        } else {
            vol[c] = v[c] as u32;
        }
    }
    crate::capture::set_gain(sw, vol);
    (sw, vol) != old
}

/// The kernel mixer's gain for `MASTER`; called with `MASTER` held so concurrent writers reach the mixer in
/// the order of their updates (`Mixer::set_master` only stores atomics, no lock is taken under it).
fn apply_master(m: &crate::thread::Mixer, (sw, vol): ([bool; 2], [u32; 2])) {
    m.set_master([vol[0] * sw[0] as u32, vol[1] * sw[1] as u32]);
}

fn notify(numid: u32) {
    // The queues are woken without `snd.ctl.subs` held (their epoll subscribers take their own locks).
    let list: Vec<Arc<Events_>> = {
        let mut subs = SUBSCRIBED.lock();
        subs.retain(|w| w.strong_count() > 0);
        subs.iter().filter_map(|w| w.upgrade()).collect()
    };
    for e in list {
        let mut q = e.q.lock();
        // Linux merges a pending event of the same element (snd_ctl_notify).
        match q.iter_mut().find(|x| x.1 == numid) {
            Some(x) => x.0 |= EVENT_MASK_VALUE,
            None => {
                if q.try_reserve(1).is_ok() {
                    q.push_back((EVENT_MASK_VALUE, numid));
                }
            }
        }
        drop(q);
        e.poll.wake_all();
        e.wq.wake_all();
    }
}

fn put_id(b: &mut [u8], at: usize, i: usize) {
    b[at..at + ELEM_ID_SIZE].fill(0);
    put_u32(b, at + elem::NUMID, i as u32 + 1);
    put_u32(b, at + elem::IFACE, ELEMS[i].iface);
    put_str(b, at + elem::NAME, elem::NAME_LEN, ELEMS[i].name);
}

/// `snd_ctl_find_id_locked` (control.c:883): by numid, else by iface/device/subdevice/name/index.
fn find(b: &[u8]) -> Option<usize> {
    let numid = get_u32(b, elem::NUMID);
    if numid != 0 {
        return (numid as usize).checked_sub(1).filter(|&i| i < ELEMS.len());
    }
    let name = &b[elem::NAME..elem::NAME + elem::NAME_LEN];
    let len = name.iter().position(|&c| c == 0).unwrap_or(elem::NAME_LEN);
    ELEMS.iter().position(|e| {
        e.iface == get_u32(b, elem::IFACE) && get_u32(b, elem::DEVICE) == 0 && get_u32(b, elem::SUBDEVICE) == 0 && get_u32(b, elem::INDEX) == 0 && e.name.as_bytes() == &name[..len]
    })
}

fn card_info(b: &mut [u8]) {
    b.fill(0);
    put_u32(b, card::CARD, 0);
    put_str(b, card::ID, 16, super::CARD_ID);
    put_str(b, card::DRIVER, 16, super::DRIVER);
    put_str(b, card::NAME, 32, super::CARD_NAME);
    put_str(b, card::LONGNAME, 80, super::LONGNAME);
    put_str(b, card::MIXERNAME, 80, super::MIXERNAME);
}

impl Ctl {
    fn new(open: bool) -> Ctl {
        Ctl { open, ev: Arc::new(Events_ { q: SpinLock::named("snd.ctl.events", VecDeque::new()), poll: PollQueue::new(), wq: WaitQueue::new() }), subscribed: SpinLock::named("snd.ctl.file", false), prefer_subdevice: SpinLock::named("snd.ctl.file", -1), calls: [const { AtomicU32::new(0) }; 256] }
    }

    fn get<const N: usize>(mem: &dyn UserMem, arg: u64) -> Result<[u8; N], Errno> {
        let mut b = [0u8; N];
        if mem.read(arg, &mut b) { Ok(b) } else { Err(EFAULT) }
    }
    fn put(mem: &dyn UserMem, arg: u64, b: &[u8]) -> Result<i64, Errno> {
        if mem.write(arg, b) { Ok(0) } else { Err(EFAULT) }
    }

    fn elem_write(&self, b: &mut [u8]) -> Result<(), Errno> {
        let i = find(b).ok_or(ENOENT)?;
        let e = &ELEMS[i];
        if e.access & ACCESS_WRITE == 0 {
            return Err(EPERM);
        }
        put_id(b, 0, i);
        let v: Vec<i64> = (0..e.count).map(|c| get_u64(b, elem::VALUE + c * 8) as i64).collect();
        // CONFIG_SND_CTL_INPUT_VALIDATION's check (sanity_check_input_values): out of range is EINVAL.
        if v.iter().any(|&x| x < e.min || x > e.max) {
            return Err(EINVAL);
        }
        if i == CAPTURE_SWITCH || i == CAPTURE_VOLUME {
            if capture_write(i, &v) {
                klog::event!(Info, "snd", "ctl.capture", numid = i + 1, left = v[0], right = v[1]);
                notify(i as u32 + 1);
            }
            return Ok(());
        }
        // Started (or found) before `MASTER` is taken: the first use spawns the mixer thread.
        let mixer = crate::mixer();
        if let Err(e) = &mixer {
            klog::event!(Warn, "snd", "ctl.master_unapplied", err = alloc::format!("{e:?}").as_str());
        }
        let changed = {
            let mut m = MASTER.lock();
            let old = *m;
            for c in 0..2 {
                if i == 0 {
                    m.0[c] = v[c] != 0;
                } else {
                    m.1[c] = v[c] as u32;
                }
            }
            if *m != old
                && let Ok(mx) = &mixer
            {
                apply_master(mx, *m);
            }
            *m != old
        };
        if changed {
            klog::event!(Info, "snd", "ctl.master", numid = i + 1, left = v[0], right = v[1]);
            notify(i as u32 + 1);
            if i == 0 {
                // USB feature units mute in hardware too (control transfers: outside `MASTER`).
                crate::usbback::master_switch(&|| MASTER.lock().0);
            }
        }
        Ok(())
    }

    /// `snd_ctl_ioctl` (control.c:2000) and `snd_pcm_control_ioctl` (pcm.c:83).
    pub fn ioctl(&self, cmd: u64, arg: u64, mem: &dyn UserMem) -> Result<i64, Errno> {
        if !self.open {
            return Err(ENXIO);
        }
        match cmd {
            CTL_PVERSION => Self::put(mem, arg, &SNDRV_CTL_VERSION.to_le_bytes()),
            CTL_CARD_INFO => {
                let mut b = [0u8; CARD_INFO_SIZE];
                card_info(&mut b);
                Self::put(mem, arg, &b)
            }
            CTL_ELEM_LIST => {
                let mut b = Self::get::<ELEM_LIST_SIZE>(mem, arg)?;
                let (mut offset, space, pids) = (get_u32(&b, elem::LIST_OFFSET) as usize, get_u32(&b, elem::LIST_SPACE), get_u64(&b, elem::LIST_PIDS));
                put_u32(&mut b, elem::LIST_COUNT, ELEMS.len() as u32);
                let mut used = 0u32;
                while used < space && offset < ELEMS.len() {
                    let mut id = [0u8; ELEM_ID_SIZE];
                    put_id(&mut id, 0, offset);
                    if !mem.write(pids.wrapping_add(used as u64 * ELEM_ID_SIZE as u64), &id) {
                        return Err(EFAULT);
                    }
                    used += 1;
                    offset += 1;
                }
                put_u32(&mut b, elem::LIST_USED, used);
                Self::put(mem, arg, &b)
            }
            CTL_ELEM_INFO => {
                let mut b = Self::get::<ELEM_INFO_SIZE>(mem, arg)?;
                let i = find(&b).ok_or(ENOENT)?;
                let e = &ELEMS[i];
                put_id(&mut b, 0, i);
                put_u32(&mut b, elem::INFO_TYPE, e.ty);
                put_u32(&mut b, elem::INFO_ACCESS, e.access);
                put_u32(&mut b, elem::INFO_COUNT, e.count as u32);
                put_u32(&mut b, elem::INFO_OWNER, u32::MAX);
                put_u64(&mut b, elem::INFO_MIN, e.min as u64);
                put_u64(&mut b, elem::INFO_MAX, e.max as u64);
                put_u64(&mut b, elem::INFO_STEP, 0);
                Self::put(mem, arg, &b)
            }
            CTL_ELEM_READ => {
                let mut b = Self::get::<ELEM_VALUE_SIZE>(mem, arg)?;
                let i = find(&b).ok_or(ENOENT)?;
                if ELEMS[i].access & ACCESS_READ == 0 {
                    return Err(EPERM);
                }
                put_id(&mut b, 0, i);
                for (c, v) in values(i).iter().enumerate() {
                    put_u64(&mut b, elem::VALUE + c * 8, *v as u64);
                }
                Self::put(mem, arg, &b)
            }
            CTL_ELEM_WRITE => {
                let mut b = Self::get::<ELEM_VALUE_SIZE>(mem, arg)?;
                self.elem_write(&mut b)?;
                Self::put(mem, arg, &b)
            }
            CTL_SUBSCRIBE_EVENTS => {
                let v = i32::from_le_bytes(Self::get::<4>(mem, arg)?);
                let mut s = self.subscribed.lock();
                if v < 0 {
                    return Self::put(mem, arg, &(*s as i32).to_le_bytes());
                }
                if v != 0 && !*s {
                    *s = true;
                    let mut subs = SUBSCRIBED.lock();
                    subs.try_reserve(1).map_err(|_| ENOMEM)?;
                    subs.push(Arc::downgrade(&self.ev));
                } else if v == 0 && *s {
                    *s = false;
                    self.ev.q.lock().clear();
                    SUBSCRIBED.lock().retain(|w| !core::ptr::eq(w.as_ptr(), Arc::as_ptr(&self.ev)));
                }
                Ok(0)
            }
            CTL_TLV_READ => {
                let b = Self::get::<8>(mem, arg)?;
                let numid = get_u32(&b, 0);
                if numid == 0 {
                    return Err(EINVAL);
                }
                if numid as usize > ELEMS.len() {
                    return Err(ENOENT);
                }
                // No element has TLV data (read_tlv_buf: no TLV_READ access).
                Err(ENXIO)
            }
            CTL_POWER => Err(ENOPROTOOPT),
            CTL_POWER_STATE => Self::put(mem, arg, &0u32.to_le_bytes()),
            CTL_PCM_NEXT_DEVICE => {
                let d = i32::from_le_bytes(Self::get::<4>(mem, arg)?);
                let next: i32 = if d < 0 { 0 } else { -1 };
                Self::put(mem, arg, &next.to_le_bytes())
            }
            CTL_PCM_INFO => {
                let mut b = Self::get::<PCM_INFO_SIZE>(mem, arg)?;
                let (device, sub, stream) = (get_u32(&b, pcm_info::DEVICE), get_u32(&b, pcm_info::SUBDEVICE), get_i32(&b, pcm_info::STREAM));
                if !(0..=1).contains(&stream) {
                    return Err(EINVAL);
                }
                if device != 0 {
                    return Err(ENXIO);
                }
                if sub != 0 {
                    return Err(ENXIO);
                }
                super::pcm_info(&mut b, stream as u32);
                Self::put(mem, arg, &b)
            }
            CTL_PCM_PREFER_SUBDEVICE => {
                *self.prefer_subdevice.lock() = i32::from_le_bytes(Self::get::<4>(mem, arg)?);
                Ok(0)
            }
            _ => Err(ENOTTY),
        }
    }
}

impl File for Ctl {
    fn open_node(&self, _flags: u64, _who: vfs::inode::Cred) -> Option<Result<Arc<dyn File>, Errno>> {
        if self.open {
            return None;
        }
        Some(super::allowed(super::CTL_NODE).map(|_| Arc::new(Ctl::new(true)) as Arc<dyn File>))
    }
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        self.read_stream(buf, true)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    /// `snd_ctl_read` (control.c:2071): whole `snd_ctl_event`s.
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
        if !*self.subscribed.lock() {
            return Err(EBADFD);
        }
        if buf.len() < CTL_EVENT_SIZE {
            return Err(EINVAL);
        }
        let mut n = 0;
        loop {
            while buf.len() - n >= CTL_EVENT_SIZE {
                let Some((mask, numid)) = self.ev.q.lock().pop_front() else { break };
                let ev = &mut buf[n..n + CTL_EVENT_SIZE];
                ev.fill(0);
                put_u32(ev, 4, mask);
                put_id(ev, 8, numid as usize - 1);
                n += CTL_EVENT_SIZE;
            }
            if n > 0 {
                return Ok(n);
            }
            if nonblock {
                return Err(EAGAIN);
            }
            if vfs::dentry::interrupted() {
                return Err(uapi::ab::siginfo::ERESTARTSYS);
            }
            // One bounded slice (B4); the loop looks at kills each time.
            let q = &self.ev;
            q.wq.wait_until_timeout(|| !q.q.lock().is_empty() || !*self.subscribed.lock(), 50_000_000);
            if !*self.subscribed.lock() {
                return Err(EBADFD);
            }
        }
    }
    /// 0666: Linux has root:audio 0660 plus the seat's ACL (uaccess); V2 has neither, B3's policy check on
    /// open is the gate.
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o666
    }
    fn rdev(&self) -> u64 {
        vfs::abi::encode_dev(super::ALSA_MAJOR, super::CTL_MINOR)
    }
    fn ioctl_user(&self, cmd: u64, arg: u64, mem: &dyn UserMem) -> Option<Result<i64, Errno>> {
        self.calls[(cmd & 0xff) as usize].fetch_add(1, Ordering::Relaxed);
        Some(self.ioctl(cmd, arg, mem))
    }
    /// `snd_ctl_poll`: readable while events wait (only when subscribed).
    fn events(&self) -> Events {
        if *self.subscribed.lock() && !self.ev.q.lock().is_empty() { Events(Events::IN | Events::RDNORM) } else { Events(0) }
    }
    fn poll_exact(&self) -> bool {
        true
    }
    fn poll_head(&self) -> Option<&PollQueue> {
        Some(&self.ev.poll)
    }
}

impl Drop for Ctl {
    fn drop(&mut self) {
        if self.open {
            let mut calls = alloc::string::String::new();
            for (nr, c) in self.calls.iter().enumerate() {
                let c = c.load(Ordering::Relaxed);
                if c > 0 {
                    calls.push_str(&alloc::format!("{}{nr:02x}:{c}", if calls.is_empty() { "" } else { "," }));
                }
            }
            klog::event!(Info, "snd", "ctl.close", ioctls = calls.as_str());
        }
        if self.open && *self.subscribed.lock() {
            SUBSCRIBED.lock().retain(|w| w.strong_count() > 0 && !core::ptr::eq(w.as_ptr(), Arc::as_ptr(&self.ev)));
        }
    }
}

pub(super) fn node() -> Arc<dyn File> {
    Arc::new(Ctl::new(false))
}
