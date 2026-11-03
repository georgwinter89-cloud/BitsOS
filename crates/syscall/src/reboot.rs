//! reboot(2) (UL-7; AB's syscall file granted to UL for it), linux-6.6/kernel/reboot.c `SYSCALL_DEFINE4(reboot)`
//! :700 and kernel/pid_namespace.c `reboot_pid_ns` :319, numbers from include/uapi/linux/reboot.h.
//!
//! Order as Linux: CAP_SYS_BOOT in the caller's pid namespace's user namespace (EPERM), the magic numbers
//! (EINVAL), a child pid namespace's init ended as if by SIGHUP (restart) or SIGINT (power off, halt) and the
//! caller with it, then the command: RESTART/RESTART2 reset through `platform::power::reboot` (the clean path
//! of the console's and MCP's reboot: write-back, mirror pause, storage stop, park, quiesce, RB-16's crumbs,
//! then the reboot plan), CAD_ON/CAD_OFF set the Ctrl+Alt+Del flag (`cad_on`), POWER_OFF and HALT switch off
//! through ACPI S5 (`platform::power::poweroff`; a halt with the picture frozen helps nobody, so HALT is
//! POWER_OFF here), KEXEC and SW_SUSPEND are EINVAL (no kexec_load image, no hibernation: Linux answers so
//! without one), anything else EINVAL.
//!
//! BitsOS on top (rule R8f): power off only from real input. POWER_OFF/HALT is EPERM for a process MCP's `run`
//! started (its program id, kept over fork and exec) and whenever the input that confirmed it was not typed
//! on the machine (`judge`): the newest Enter or mouse click in the input sink's look-back must be from a real
//! device, no key injected (MCP `send_keys`/`send_mouse`, a program writing /dev/input) since the click or
//! Enter before it (when that one has left the look-back, the line's start is unknown: refused too), and no
//! older than `FRESH_NS`. Init asks the same question before it ends any program: `/proc/bitsos/power`, whose
//! third line also lists the processes of MCP's `run` (init refuses a power-off they ask for over its socket
//! or by a signal). A power-off that did not happen returns EIO (Linux never returns; the drivers stay
//! quiesced, "hold the power button" was logged).

use crate::SysCtx;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use console::evdev::InputEvent;
use core::sync::atomic::{AtomicBool, Ordering};
use uapi::errno::*;

pub const MAGIC1: u32 = 0xfee1_dead;
pub const MAGIC2: u32 = 672_274_793;
pub const MAGIC2A: u32 = 85_072_278;
pub const MAGIC2B: u32 = 369_367_448;
pub const MAGIC2C: u32 = 537_993_216;

pub const CMD_RESTART: u32 = 0x0123_4567;
pub const CMD_HALT: u32 = 0xCDEF_0123;
pub const CMD_CAD_ON: u32 = 0x89AB_CDEF;
pub const CMD_CAD_OFF: u32 = 0x0000_0000;
pub const CMD_POWER_OFF: u32 = 0x4321_FEDC;
pub const CMD_RESTART2: u32 = 0xA1B2_C3D4;
pub const CMD_SW_SUSPEND: u32 = 0xD000_FCE2;
pub const CMD_KEXEC: u32 = 0x4558_4543;

/// `CAP_SYS_BOOT` (include/uapi/linux/capability.h; `proc::cred::Cap` does not name it).
const CAP_SYS_BOOT: u32 = 22;
// The program id MCP's `run` gives what it starts (UL-7 CR, exported by proc since INT-W27).
use proc::MCP_PROGRAM;
/// RESTART2's argument: `char buffer[256]`, at most 255 bytes copied.
const RESTART2_MAX: usize = 255;
/// How old the confirming Enter or click may be (R8f): long enough for init to end the programs and write
/// everything back, short enough that a key typed long ago confirms nothing.
pub const FRESH_NS: u64 = 60_000_000_000;

const SIGHUP: u32 = 1;
const SIGINT: u32 = 2;
const EV_KEY: u16 = 0x01;
const KEY_ENTER: u16 = 28;
const KEY_KPENTER: u16 = 96;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_TOUCH: u16 = 0x14a;

/// Linux `C_A_D` (on by default: Ctrl+Alt+Del restarts at once; off: init gets SIGINT).
static CAD: AtomicBool = AtomicBool::new(true);
/// One restart or power-off at a time (Linux `system_transition_mutex`).
static TRANSITION: AtomicBool = AtomicBool::new(false);

/// Whether Ctrl+Alt+Del should restart at once (CAD_ON) rather than signal init (CAD_OFF), for the keyboard.
pub fn cad_on() -> bool {
    CAD.load(Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmd {
    Restart,
    Restart2,
    Halt,
    PowerOff,
    CadOn,
    CadOff,
    SwSuspend,
    Kexec,
}

pub fn magic_ok(m1: u32, m2: u32) -> bool {
    m1 == MAGIC1 && [MAGIC2, MAGIC2A, MAGIC2B, MAGIC2C].contains(&m2)
}

pub fn decode(cmd: u32) -> Option<Cmd> {
    Some(match cmd {
        CMD_RESTART => Cmd::Restart,
        CMD_RESTART2 => Cmd::Restart2,
        CMD_HALT => Cmd::Halt,
        CMD_POWER_OFF => Cmd::PowerOff,
        CMD_CAD_ON => Cmd::CadOn,
        CMD_CAD_OFF => Cmd::CadOff,
        CMD_SW_SUSPEND => Cmd::SwSuspend,
        CMD_KEXEC => Cmd::Kexec,
        _ => return None,
    })
}

/// `reboot_pid_ns`: the signal a child namespace's init ends with, None (EINVAL) for any other command.
pub fn pid_ns_signal(cmd: Option<Cmd>) -> Option<u32> {
    match cmd? {
        Cmd::Restart | Cmd::Restart2 => Some(SIGHUP),
        Cmd::PowerOff | Cmd::Halt => Some(SIGINT),
        _ => None,
    }
}

/// Why a power-off is refused (R8f).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The caller was started by MCP's `run` (or descends from such a process).
    McpRun,
    /// No Enter or click in the input look-back.
    NoInput,
    /// The newest Enter or click was injected.
    InjectedConfirm,
    /// Some key or button since the previous Enter or click was injected.
    InjectedKeys,
    /// The previous Enter or click has left the look-back (much input since), so the line's keys are unknown.
    Unclear,
    /// The newest Enter or click is older than `FRESH_NS`.
    Stale { age_ms: u64 },
}

impl Refusal {
    pub fn word(self) -> &'static str {
        match self {
            Refusal::McpRun => "mcp-run",
            Refusal::NoInput => "no-input",
            Refusal::InjectedConfirm => "injected-confirm",
            Refusal::InjectedKeys => "injected-keys",
            Refusal::Unclear => "unclear",
            Refusal::Stale { .. } => "stale",
        }
    }
    pub fn text(self) -> String {
        match self {
            Refusal::McpRun => String::from("the request comes from a program MCP's run started"),
            Refusal::NoInput => String::from("no Enter or click was typed on the machine's own keyboard or mouse"),
            Refusal::InjectedConfirm => String::from("the Enter or click came from MCP or a program (injected input)"),
            Refusal::InjectedKeys => String::from("keys of the line came from MCP or a program (injected input)"),
            Refusal::Unclear => String::from("too much input since the previous Enter or click to tell where the line came from: type it again"),
            Refusal::Stale { age_ms } => format!("the last Enter or click typed on the machine is {} s old", age_ms / 1000),
        }
    }
}

fn confirms(ev: &InputEvent) -> bool {
    ev.kind == EV_KEY && ev.value != 0 && matches!(ev.code, KEY_ENTER | KEY_KPENTER | BTN_LEFT | BTN_RIGHT | BTN_MIDDLE | BTN_TOUCH)
}

/// R8f's input judgement over the sink's look-back (oldest first): Ok(age of the confirming Enter or click in
/// ms) when it was typed on the machine and nothing injected went into the line it ends. `complete`: the
/// look-back still starts at the first event since boot (nothing has left it); otherwise a line whose start
/// (the previous Enter or click) is no longer in it may hold injected keys nobody can see any more.
pub fn judge(events: &[InputEvent], complete: bool, now_ns: u64) -> Result<u64, Refusal> {
    let mut idx = events.iter().enumerate().filter(|(_, e)| confirms(e)).map(|(i, _)| i).rev();
    let Some(last) = idx.next() else { return Err(Refusal::NoInput) };
    let prev = idx.next();
    let from = prev.map_or(0, |i| i + 1);
    let ev = &events[last];
    if ev.injected {
        return Err(Refusal::InjectedConfirm);
    }
    if events[from..last].iter().any(|e| e.kind == EV_KEY && e.value != 0 && e.injected) {
        return Err(Refusal::InjectedKeys);
    }
    if prev.is_none() && !complete {
        return Err(Refusal::Unclear);
    }
    let age = now_ns.saturating_sub(ev.time_ns);
    if age > FRESH_NS {
        return Err(Refusal::Stale { age_ms: age / 1_000_000 });
    }
    Ok(age / 1_000_000)
}

/// The judgement on the live input sink.
pub fn input_verdict() -> Result<u64, Refusal> {
    let seen = console::evdev::sink().recent_since(0);
    // Sequence numbers start at 0 and every event enters the look-back: a first one above 0 means some left.
    let complete = seen.first().is_none_or(|(s, _)| *s == 0);
    let events: Vec<InputEvent> = seen.into_iter().map(|(_, e)| e).collect();
    judge(&events, complete, time::now_ns())
}

/// Whether `p` may power the machine off now (R8f).
pub fn power_off_verdict(program: policy::ProgramId) -> Result<u64, Refusal> {
    if program == MCP_PROGRAM {
        return Err(Refusal::McpRun);
    }
    input_verdict()
}

/// `/proc/bitsos/power`: line 1 machine words, line 2 the reason in words, line 3 `mcp_run=` and the pids of
/// MCP's `run` processes, comma-separated (init reads all three: a power-off one of them asks is refused).
pub fn render(v: Result<u64, Refusal>, cad: bool, mcp_run: &[u32]) -> String {
    let cad = cad as u8;
    let pids: Vec<String> = mcp_run.iter().map(|p| format!("{p}")).collect();
    let pids = pids.join(",");
    match v {
        Ok(age_ms) => format!("poweroff=allowed age_ms={age_ms} cad={cad}\nthe last Enter or click was typed on the machine\nmcp_run={pids}\n"),
        Err(r) => format!("poweroff=refused why={} cad={cad}\n{}\nmcp_run={pids}\n", r.word(), r.text()),
    }
}

/// The processes of MCP's `run` (the program id is kept over fork and exec), ascending.
pub fn mcp_run_pids() -> Vec<u32> {
    let mut v: Vec<u32> = proc::processes().iter().filter(|p| p.program == MCP_PROGRAM).map(|p| p.pid).collect();
    v.sort_unstable();
    v
}

fn proc_render() -> String {
    render(input_verdict(), cad_on(), &mcp_run_pids())
}
vfs::procfile!("/proc/bitsos/power", proc_render);

/// `ns_capable(pid_ns->user_ns, CAP_SYS_BOOT)` within the thread's bounding set (as uts.rs asks CAP_SYS_ADMIN).
fn may_boot(c: &SysCtx<'_>) -> bool {
    proc::cred::ns_capable_raw(&c.proc.cred(), &c.proc.pid_ns.user_ns, CAP_SYS_BOOT) && c.thread.prctl().cap_bset() & (1u64 << CAP_SYS_BOOT) != 0
}

/// `reboot_pid_ns`: the init of the child namespace `ns` ends as if by `sig` and every member with it. Linux
/// stores `sig` in `pid_ns->reboot` and SIGKILLs the reaper, which then exits with that signal (its parent sees
/// WTERMSIG = sig); `Group::kill` is that forced end with the signal as the status (it does not go through
/// the signal queue, so the namespace init's SIGNAL_UNKILLABLE cannot drop it), and the init's end zaps the
/// namespace (`zap_pid_ns_processes`). False when the namespace has no init left.
pub fn end_pid_ns(ns: &proc::ns::pid::PidNs, sig: u32, by: u32) -> bool {
    let Some(init) = proc::find(ns.reaper()) else { return false };
    klog::event!(Info, "ul", "reboot.pid_ns", pid = by, init = init.pid, signal = sig);
    init.kill(sig);
    true
}

/// RESTART2's argument as Linux copies it (`strncpy_from_user(buffer, arg, sizeof(buffer) - 1)`): up to the
/// NUL or 255 bytes, whichever comes first, no error when no NUL is within them; EFAULT when a byte before
/// that end cannot be read.
pub fn restart2_arg(mm: &mm::AddressSpace, arg: u64) -> Result<String, i64> {
    let bytes = match mm.read_cstr(arg, RESTART2_MAX) {
        Ok(b) => b,
        // No NUL within 255 bytes, or a fault before it: the 255 bytes are all readable only in the first case.
        Err(_) => {
            let mut b = alloc::vec![0u8; RESTART2_MAX];
            mm.copy_from_user(&mut b, arg).map_err(|_| -EFAULT)?;
            b
        }
    };
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Takes the transition (waits while another restart or power-off runs, as Linux's mutex does; each wait is
/// bounded and the first one either never returns or gives it back).
fn begin() {
    while TRANSITION.swap(true, Ordering::AcqRel) {
        sched::sleep_ns(100_000_000);
    }
}

fn sys_reboot(c: &mut SysCtx<'_>) -> i64 {
    let (m1, m2, raw, arg) = (c.args[0] as u32, c.args[1] as u32, c.args[2] as u32, c.args[3]);
    let pid = c.proc.pid;
    if !may_boot(c) {
        klog::event!(Info, "ul", "reboot.refused", pid = pid, cmd = raw, reason = "no CAP_SYS_BOOT");
        return -EPERM;
    }
    if !magic_ok(m1, m2) {
        return -EINVAL;
    }
    let cmd = decode(raw);
    let ns = c.proc.pid_ns.clone();
    if !ns.is_init() {
        let Some(sig) = pid_ns_signal(cmd) else { return -EINVAL };
        // The namespace ends, and the caller with it (Linux `do_exit(0)` after `reboot_pid_ns`).
        end_pid_ns(&ns, sig, pid);
        c.thread.exit_thread(0);
        return 0;
    }
    let Some(cmd) = cmd else { return -EINVAL };
    match cmd {
        Cmd::CadOn | Cmd::CadOff => {
            CAD.store(cmd == Cmd::CadOn, Ordering::Relaxed);
            klog::event!(Info, "ul", "reboot.cad", pid = pid, on = cmd == Cmd::CadOn);
            0
        }
        Cmd::Kexec | Cmd::SwSuspend => -EINVAL,
        Cmd::Restart | Cmd::Restart2 => {
            let why = if cmd == Cmd::Restart2 {
                match restart2_arg(&c.proc.mm, arg) {
                    Ok(s) => s,
                    Err(e) => return e,
                }
            } else {
                String::new()
            };
            begin();
            klog::event!(Warn, "ul", "reboot.restart", pid = pid, name = c.proc.name.as_str(), arg = why.as_str(); "reboot(2): restarting");
            platform::power::reboot()
        }
        Cmd::PowerOff | Cmd::Halt => {
            if let Err(r) = power_off_verdict(c.proc.program) {
                klog::event!(Warn, "ul", "reboot.refused", pid = pid, cmd = raw, reason = r.word(); "power-off refused (R8f): only real input switches off");
                return -EPERM;
            }
            begin();
            // Judged again: a wait for another transition may have outlived the first judgement.
            if let Err(r) = power_off_verdict(c.proc.program) {
                TRANSITION.store(false, Ordering::Release);
                klog::event!(Warn, "ul", "reboot.refused", pid = pid, cmd = raw, reason = r.word(); "power-off refused (R8f): only real input switches off");
                return -EPERM;
            }
            klog::event!(Warn, "ul", "reboot.poweroff", pid = pid, name = c.proc.name.as_str(), halt = cmd == Cmd::Halt; "reboot(2): switching off");
            let e = platform::power::poweroff();
            TRANSITION.store(false, Ordering::Release);
            klog::event!(Error, "ul", "reboot.poweroff_failed", pid = pid, error = format!("{e:?}"); "power-off did not happen: hold the power button");
            -EIO
        }
    }
}
crate::syscall_nr!(169, "reboot", sys_reboot);

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: u16, value: i32, injected: bool, t: u64) -> InputEvent {
        InputEvent { time_ns: t, kind: EV_KEY, code, value, device: 1, injected }
    }
    const S: u64 = 1_000_000_000;

    #[test]
    fn magic_and_commands_as_linux() {
        assert!(magic_ok(0xfee1dead, 672274793) && magic_ok(0xfee1dead, 85072278) && magic_ok(0xfee1dead, 369367448));
        assert!(magic_ok(0xfee1dead, 537993216));
        assert!(!magic_ok(0xfee1deae, 672274793) && !magic_ok(0xfee1dead, 0));
        assert_eq!(decode(0x01234567), Some(Cmd::Restart));
        assert_eq!(decode(0), Some(Cmd::CadOff));
        assert_eq!(decode(0x4321FEDC), Some(Cmd::PowerOff));
        assert_eq!(decode(0xCDEF0123), Some(Cmd::Halt));
        assert_eq!(decode(0x12345678), None);
        assert_eq!(pid_ns_signal(Some(Cmd::Restart2)), Some(1));
        assert_eq!(pid_ns_signal(Some(Cmd::Halt)), Some(2));
        assert_eq!(pid_ns_signal(Some(Cmd::CadOn)), None);
        assert_eq!(pid_ns_signal(None), None);
    }

    #[test]
    fn typed_line_allows() {
        let ev = [key(25, 1, false, S), key(25, 0, false, S), key(KEY_ENTER, 1, false, 2 * S), key(KEY_ENTER, 0, false, 2 * S)];
        assert_eq!(judge(&ev, true, 3 * S), Ok(1000));
        let click = [key(30, 1, true, S), key(KEY_ENTER, 1, true, S), key(BTN_LEFT, 1, false, 2 * S)];
        assert_eq!(judge(&click, true, 2 * S), Ok(0), "a real click after an injected line confirms only itself");
    }

    #[test]
    fn injected_or_old_refuses() {
        assert_eq!(judge(&[], true, S), Err(Refusal::NoInput));
        assert_eq!(judge(&[key(30, 1, false, S)], true, S), Err(Refusal::NoInput));
        let sent = [key(KEY_ENTER, 1, false, S), key(25, 1, true, 2 * S), key(KEY_ENTER, 1, true, 2 * S)];
        assert_eq!(judge(&sent, true, 3 * S), Err(Refusal::InjectedConfirm));
        let mixed = [key(KEY_ENTER, 1, false, S), key(25, 1, true, 2 * S), key(25, 0, true, 2 * S), key(KEY_KPENTER, 1, false, 3 * S)];
        assert_eq!(judge(&mixed, true, 3 * S), Err(Refusal::InjectedKeys));
        let earlier = [key(25, 1, true, S), key(KEY_ENTER, 1, false, 2 * S), key(24, 1, false, 3 * S), key(KEY_ENTER, 1, false, 4 * S)];
        assert_eq!(judge(&earlier, true, 4 * S), Ok(0), "injected keys of an earlier line do not count");
        let old = [key(KEY_ENTER, 1, false, S)];
        assert_eq!(judge(&old, true, S + FRESH_NS + S), Err(Refusal::Stale { age_ms: 61_000 }));
        let after = [key(KEY_ENTER, 1, false, S), key(30, 1, true, 2 * S)];
        assert_eq!(judge(&after, true, 2 * S), Ok(1000), "keys after the Enter belong to the next line");
    }

    #[test]
    fn line_start_out_of_the_look_back() {
        // The injected "poweroff" of the line left the look-back (much mouse input since): its start is unknown.
        let only = [key(BTN_LEFT, 0, false, S), key(KEY_ENTER, 1, false, 2 * S)];
        assert_eq!(judge(&only, false, 2 * S), Err(Refusal::Unclear));
        assert_eq!(judge(&only, true, 2 * S), Ok(0), "nothing left the look-back: the first line since boot");
        let both = [key(KEY_ENTER, 1, false, S), key(25, 1, false, S), key(KEY_ENTER, 1, false, 2 * S)];
        assert_eq!(judge(&both, false, 2 * S), Ok(0), "the line's start is still seen");
        let inj = [key(25, 1, true, S), key(KEY_ENTER, 1, false, 2 * S)];
        assert_eq!(judge(&inj, false, 2 * S), Err(Refusal::InjectedKeys), "seen injected keys name themselves");
    }

    #[test]
    fn proc_text() {
        assert_eq!(render(Ok(420), true, &[]), "poweroff=allowed age_ms=420 cad=1\nthe last Enter or click was typed on the machine\nmcp_run=\n");
        let t = render(Err(Refusal::InjectedConfirm), false, &[7, 12]);
        assert!(t.starts_with("poweroff=refused why=injected-confirm cad=0\n") && t.contains("injected input"), "{t}");
        assert!(t.ends_with("\nmcp_run=7,12\n"), "{t}");
        assert_eq!(power_off_verdict(MCP_PROGRAM), Err(Refusal::McpRun));
    }
}
