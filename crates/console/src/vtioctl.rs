//! The VT ioctls beyond the screen and keyboard modes (IN-4 fixer): what VT tools and seat managers ask of
//! /dev/tty0 before KDSETMODE/KDSKBMODE (libseat's builtin backend and seatd, weston and wlroots without logind,
//! busybox openvt/chvt/deallocvt/kbd_mode, setleds). After linux-6.6/drivers/tty/vt/vt_ioctl.c (`vt_k_ioctl`
//! :269, `vt_ioctl` :725, `vt_reldisp` :555), keyboard.c (`vt_do_kdskled` :2112, `vt_do_kdskbmeta` :1880,
//! `kbd_rate` :312) and vt.c (`tioclinux` :3143).
//!
//! BitsOS has one VT (tty1, the console): it is always the active one and in use, so VT_OPENQRY finds no free VT
//! (-1, Linux's answer when all are taken), VT_ACTIVATE/VT_WAITACTIVE of VT 1 answer at once and of any other VT
//! ENXIO (it cannot be allocated), VT_RELDISP never has a switch pending. VT_SETMODE is stored and answered by
//! VT_GETMODE. KDSETLED stores the lights a program asked for (KDGETLED answers them) without driving the
//! keyboards' LEDs; KDSKBLED sets the lock state every keyboard shows. Sound (KIOCSOUND, KDMKTONE): no speaker,
//! accepted. Not built, ENOTTY as for an unknown command: the keymap and font ioctls (KDGKBENT, KDFONTOP,
//! PIO_*/GIO_*), VT_RESIZE(X), VT_WAITEVENT, VT_SETACTIVATE of another VT.

use crate::jobs::Caller;
use core::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use ksync::SpinLock;
use uapi::errno::*;
use vfs::Errno;
use vfs::sys::UserMem;

pub const KIOCSOUND: u64 = 0x4B2F;
pub const KDMKTONE: u64 = 0x4B30;
pub const KDGETLED: u64 = 0x4B31;
pub const KDSETLED: u64 = 0x4B32;
pub const KDSIGACCEPT: u64 = 0x4B4E;
pub const KDKBDREP: u64 = 0x4B52;
pub const KDGKBMETA: u64 = 0x4B62;
pub const KDSKBMETA: u64 = 0x4B63;
pub const KDGKBLED: u64 = 0x4B64;
pub const KDSKBLED: u64 = 0x4B65;
pub const K_METABIT: u8 = 0x03;
pub const K_ESCPREFIX: u8 = 0x04;
pub const TIOCLINUX: u64 = 0x541C;
pub const VT_OPENQRY: u64 = 0x5600;
pub const VT_GETMODE: u64 = 0x5601;
pub const VT_SETMODE: u64 = 0x5602;
pub const VT_GETSTATE: u64 = 0x5603;
pub const VT_RELDISP: u64 = 0x5605;
pub const VT_ACTIVATE: u64 = 0x5606;
pub const VT_WAITACTIVE: u64 = 0x5607;
pub const VT_DISALLOCATE: u64 = 0x5608;
pub const VT_LOCKSWITCH: u64 = 0x560B;
pub const VT_UNLOCKSWITCH: u64 = 0x560C;
pub const VT_GETHIFONTMASK: u64 = 0x560D;
pub const VT_SETACTIVATE: u64 = 0x560F;
pub const VT_AUTO: u8 = 0;
pub const VT_PROCESS: u8 = 1;
pub const VT_ACKACQ: u64 = 2;
/// `MAX_NR_CONSOLES`: the VT numbers a program may name.
const MAX_NR_CONSOLES: u64 = 63;
/// `struct vt_mode`: mode, waitv (chars), relsig, acqsig, frsig (shorts).
pub const VT_MODE_SIZE: usize = 8;
const SIGKILL: u64 = 9;
const NSIG: u64 = 64;

/// VT_SETMODE's `struct vt_mode` (frsig kept 0 as Linux) and the process that set it.
static VT_MODE: SpinLock<([u8; VT_MODE_SIZE], u32)> = SpinLock::named("vt.vtmode", ([0; VT_MODE_SIZE], 0));
/// KDSKBLED's default flags (the high nibble of KDGKBLED).
static DEFAULT_LEDFLAGS: AtomicU8 = AtomicU8::new(0);
/// KDSETLED's lights (`LED_SHOW_IOCTL`), 0xFF = the lights follow the lock flags (`LED_SHOW_FLAGS`).
static LED_IOCTL: AtomicU8 = AtomicU8::new(0xFF);
static META: AtomicU8 = AtomicU8::new(K_METABIT);
/// KDSIGACCEPT: (pid, signal) for the keyboard's spawn request (no key sends it on BitsOS).
static SPAWN: AtomicU32 = AtomicU32::new(0);
static LOCKED_SWITCH: AtomicU8 = AtomicU8::new(0);

fn put(mem: &dyn UserMem, arg: u64, data: &[u8]) -> Result<i64, Errno> {
    if mem.write(arg, data) { Ok(0) } else { Err(EFAULT) }
}

/// Linux's K_SCROLLLOCK/K_NUMLOCK/K_CAPSLOCK (= LED_SCR/LED_NUM/LED_CAP) from the sink's lock bits.
fn flags_from_locks(locks: u32) -> u8 {
    let has = |l: u16| locks & (1 << l) != 0;
    (has(crate::evdev::LED_SCROLLL) as u8) | (has(crate::evdev::LED_NUML) as u8) << 1 | (has(crate::evdev::LED_CAPSL) as u8) << 2
}
fn locks_from_flags(f: u8) -> u32 {
    let mut l = 0;
    if f & 1 != 0 {
        l |= 1 << crate::evdev::LED_SCROLLL;
    }
    if f & 2 != 0 {
        l |= 1 << crate::evdev::LED_NUML;
    }
    if f & 4 != 0 {
        l |= 1 << crate::evdev::LED_CAPSL;
    }
    l
}

/// Linux `shift_state` bits from the held keys (KG_SHIFT, KG_ALTGR, KG_CTRL, KG_ALT, KG_SHIFTL.. KG_CTRLR).
fn shift_state() -> u8 {
    let s = crate::evdev::sink();
    let h = |c: u16| s.key_held(c) as u8;
    let (ls, rs, lc, rc, la, ra) = (h(42), h(54), h(29), h(97), h(56), h(100));
    (ls | rs) | ra << 1 | (lc | rc) << 2 | la << 3 | ls << 4 | rs << 5 | lc << 6 | rc << 7
}

/// One VT ioctl, or None when `cmd` is not one of them. `perm`: `vt_ioctl`'s permission (the caller's
/// controlling terminal or CAP_SYS_TTY_CONFIG).
pub fn ioctl(cmd: u64, arg: u64, mem: &dyn UserMem, me: Option<&Caller>, perm: bool) -> Option<Result<i64, Errno>> {
    let root = me.is_some_and(|m| m.privileged());
    let r = match cmd {
        KIOCSOUND | KDMKTONE => {
            if perm {
                Ok(0)
            } else {
                Err(EPERM)
            }
        }
        KDKBDREP => kbdrep(arg, mem, root),
        KDGKBMETA => put(mem, arg, &(META.load(Ordering::Acquire) as i32).to_le_bytes()),
        KDSKBMETA => match arg as u8 {
            _ if arg > 0xFF => Err(EINVAL),
            K_METABIT | K_ESCPREFIX => {
                META.store(arg as u8, Ordering::Release);
                Ok(0)
            }
            _ => Err(EINVAL),
        },
        KDGKBLED => {
            let v = flags_from_locks(crate::evdev::sink().locks()) | DEFAULT_LEDFLAGS.load(Ordering::Acquire) << 4;
            put(mem, arg, &[v])
        }
        KDSKBLED => {
            if !perm {
                Err(EPERM)
            } else if arg & !0x77 != 0 {
                Err(EINVAL)
            } else {
                DEFAULT_LEDFLAGS.store((arg >> 4) as u8 & 7, Ordering::Release);
                crate::evdev::sink().set_locks(locks_from_flags(arg as u8 & 7));
                Ok(0)
            }
        }
        KDGETLED => {
            let l = LED_IOCTL.load(Ordering::Acquire);
            put(mem, arg, &[if l == 0xFF { flags_from_locks(crate::evdev::sink().locks()) } else { l }])
        }
        KDSETLED => {
            if !perm {
                Err(EPERM)
            } else {
                // `setledstate`: 0..7 = these lights, anything else = the lights follow the flags again.
                LED_IOCTL.store(if arg & !7 == 0 { arg as u8 } else { 0xFF }, Ordering::Release);
                Ok(0)
            }
        }
        KDSIGACCEPT => {
            if !perm || !root {
                Err(EPERM)
            } else if !(1..=NSIG).contains(&arg) || arg == SIGKILL {
                Err(EINVAL)
            } else {
                SPAWN.store(me.map(|m| m.pid).unwrap_or(0), Ordering::Release);
                Ok(0)
            }
        }
        TIOCLINUX => tioclinux(arg, mem, perm || root, root),
        VT_SETMODE => {
            if !perm {
                return Some(Err(EPERM));
            }
            let mut m = [0u8; VT_MODE_SIZE];
            if !mem.read(arg, &mut m) {
                return Some(Err(EFAULT));
            }
            if m[0] != VT_AUTO && m[0] != VT_PROCESS {
                return Some(Err(EINVAL));
            }
            m[6] = 0;
            m[7] = 0;
            *VT_MODE.lock() = (m, me.map(|m| m.pid).unwrap_or(0));
            klog::event!(Info, "console", "vt.vtmode", mode = m[0], pid = me.map(|m| m.pid).unwrap_or(0));
            Ok(0)
        }
        VT_GETMODE => {
            let m = VT_MODE.lock().0;
            put(mem, arg, &m)
        }
        VT_GETSTATE => {
            // v_active = 1; v_signal untouched; v_state: bit 0 (/dev/tty0) and bit 1 (tty1 in use).
            if !mem.write(arg, &1u16.to_le_bytes()) {
                return Some(Err(EFAULT));
            }
            put(mem, arg + 4, &3u16.to_le_bytes())
        }
        VT_OPENQRY => put(mem, arg, &(-1i32).to_le_bytes()),
        VT_ACTIVATE | VT_WAITACTIVE => {
            if !perm {
                Err(EPERM)
            } else if arg == 0 || arg > MAX_NR_CONSOLES {
                Err(ENXIO)
            } else if arg == 1 {
                Ok(0)
            } else {
                Err(ENXIO)
            }
        }
        VT_SETACTIVATE => {
            if !perm {
                return Some(Err(EPERM));
            }
            let mut b = [0u8; 4 + VT_MODE_SIZE];
            if !mem.read(arg, &mut b) {
                return Some(Err(EFAULT));
            }
            let console = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64;
            if console == 0 || console > MAX_NR_CONSOLES || console != 1 {
                return Some(Err(ENXIO));
            }
            let mut m = [0u8; VT_MODE_SIZE];
            m.copy_from_slice(&b[4..]);
            m[6] = 0;
            m[7] = 0;
            *VT_MODE.lock() = (m, me.map(|m| m.pid).unwrap_or(0));
            Ok(0)
        }
        VT_RELDISP => {
            if !perm {
                Err(EPERM)
            } else if VT_MODE.lock().0[0] != VT_PROCESS {
                Err(EINVAL)
            } else if arg == VT_ACKACQ {
                // No switch is ever pending: only the acquire acknowledgement is valid.
                Ok(0)
            } else {
                Err(EINVAL)
            }
        }
        // VT 1 is the console and always busy; the others are never allocated.
        VT_DISALLOCATE => match arg {
            _ if arg > MAX_NR_CONSOLES => Err(ENXIO),
            1 => Err(EBUSY),
            _ => Ok(0),
        },
        VT_LOCKSWITCH | VT_UNLOCKSWITCH => {
            if !root {
                Err(EPERM)
            } else {
                LOCKED_SWITCH.store((cmd == VT_LOCKSWITCH) as u8, Ordering::Release);
                Ok(0)
            }
        }
        VT_GETHIFONTMASK => put(mem, arg, &0u16.to_le_bytes()),
        _ => return None,
    };
    Some(r)
}

/// The VT_PROCESS owner ended: back to VT_AUTO (Linux `reset_vc` on the next switch; nothing ever switches here).
pub fn reclaim(alive: &dyn Fn(u32) -> bool) {
    let mut m = VT_MODE.lock();
    if m.1 != 0 && !alive(m.1) {
        *m = ([0; VT_MODE_SIZE], 0);
    }
}

/// KDKBDREP (`kbd_rate`): delay and period above 0 change the console's repeat and every evdev keyboard's; the
/// answer is the settings now in use (the console's).
fn kbdrep(arg: u64, mem: &dyn UserMem, root: bool) -> Result<i64, Errno> {
    if !root {
        return Err(EPERM);
    }
    let mut b = [0u8; 8];
    if !mem.read(arg, &mut b) {
        return Err(EFAULT);
    }
    let delay = i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let period = i32::from_le_bytes([b[4], b[5], b[6], b[7]]);
    let (mut d, mut p) = crate::repeat::rate_ms();
    if delay > 0 {
        d = delay as u32;
    }
    if period > 0 {
        p = period as u32;
    }
    if delay > 0 || period > 0 {
        crate::repeat::set_rate_ms(d, p);
        for dev in crate::evdev::devices().iter().filter(|x| x.info.caps.repeat) {
            dev.set_repeat(d, p);
        }
        klog::event!(Info, "console", "vt.kbdrep", delay_ms = d, period_ms = p);
    }
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&(d as i32).to_le_bytes());
    out[4..].copy_from_slice(&(p as i32).to_le_bytes());
    put(mem, arg, &out)
}

/// TIOCLINUX: the first byte of the argument selects the call.
fn tioclinux(arg: u64, mem: &dyn UserMem, perm: bool, root: bool) -> Result<i64, Errno> {
    if !perm {
        return Err(EPERM);
    }
    let mut t = [0u8; 1];
    if !mem.read(arg, &mut t) {
        return Err(EFAULT);
    }
    match t[0] {
        // TIOCL_UNBLANKSCREEN, TIOCL_BLANKSCREEN: the console does not blank.
        4 | 14 => Ok(0),
        6 => put(mem, arg, &[shift_state()]),
        // TIOCL_GETMOUSEREPORTING, TIOCL_GETKMSGREDIRECT: no mouse reporting, kernel messages on the active VT.
        7 | 17 => put(mem, arg, &[0]),
        // TIOCL_SETVESABLANK reads its mode byte.
        10 => {
            let mut b = [0u8; 1];
            if mem.read(arg + 1, &mut b) { Ok(0) } else { Err(EFAULT) }
        }
        11 => {
            if !root {
                return Err(EPERM);
            }
            let mut b = [0u8; 1];
            if mem.read(arg + 1, &mut b) { Ok(0) } else { Err(EFAULT) }
        }
        // TIOCL_GETFGCONSOLE: the foreground VT's index (tty1 = 0).
        12 => Ok(0),
        13 => {
            let mut b = [0u8; 4];
            if mem.read(arg + 4, &mut b) { Ok(0) } else { Err(EFAULT) }
        }
        // TIOCL_BLANKEDSCREEN: none.
        15 => Ok(0),
        // The selection calls (SETSEL, PASTESEL, SELLOADLUT) are not built; unknown types: EINVAL.
        _ => Err(EINVAL),
    }
}
