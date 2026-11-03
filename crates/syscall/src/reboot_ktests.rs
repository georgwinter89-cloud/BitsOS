//! UL-7 ktests of reboot(2) (`reboot.rs`). Each test plays a held process (as the AB tests do: registers set,
//! `dispatch` called on the test's thread), so no user code runs.
//!
//! - `ul.reboot.errors` (Safe, rerunnable): EPERM without CAP_SYS_BOOT before anything else, EINVAL for bad
//!   magic numbers and unknown commands, KEXEC and SW_SUSPEND EINVAL, CAD_OFF/CAD_ON with every magic2 (the
//!   flag is put back). No call can reach a restart or a power-off even if a check were wrong: every command
//!   that gets past the checks is CAD or invalid.
//! - `ul.reboot.power_off_refused` (QemuOnly: it pushes keys into the console's input, and a wrong gate would
//!   switch the machine off): POWER_OFF and HALT are EPERM for a process of MCP's `run`, after an injected
//!   Enter, after injected keys ended by a real Enter, and `/proc/bitsos/power` says why; a real Enter makes
//!   the judgement allow (no power-off is called then).
//! - `ul.reboot.restart2_arg` (Safe): RESTART2's argument read as Linux's strncpy_from_user (255 bytes without
//!   a NUL are no error, a fault before the NUL or the 255th byte is EFAULT); only the reader is called.
//! - `ul.reboot.restart2_efault` (QemuOnly: a wrong order would restart): RESTART2 with an unreadable or a
//!   kernel pointer is EFAULT from the real dispatcher.
//! - `ul.reboot.pid_ns` (Safe): `reboot_pid_ns` on a child pid namespace of the test's own: its init (which
//!   no signal from inside reaches) ends with SIGHUP as its status, seen by its parent's wait4, and the other
//!   member goes with it. Only that namespace ends.

use crate::reboot;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use console::evdev::InputEvent;
use hal::entry::UserContext;
use ktest::{TestResult, kassert, ktest};
use policy::ProgramId;
use uapi::errno::*;

const SEC: u64 = 1_000_000_000;
const ENTRY: u64 = 0x40_0078;
/// exit_group(0) at the entry; at ENTRY + 9 a loop of nanosleep({100, 0}) (the clone children's sleeper).
const CODE: [u8; 27] = [
    0xb8, 0xe7, 0, 0, 0, 0x31, 0xff, 0x0f, 0x05, // mov eax, 231; xor edi, edi; syscall
    0x6a, 0x00, 0x6a, 0x64, 0x48, 0x89, 0xe7, 0x31, 0xf6, // push 0; push 100; mov rdi, rsp; xor esi, esi
    0xb8, 0x23, 0, 0, 0, 0x0f, 0x05, 0xeb, 0xee, // mov eax, 35; syscall; jmp back to the pushes
];
const SLEEP_AT: u64 = ENTRY + 9;
const SCRATCH: u64 = 1 << 16;
const NR: usize = 169;

struct Held {
    p: Arc<proc::Process>,
    th: Arc<proc::Thread>,
    ctx: UserContext,
    mem: u64,
    _pin: proc::MmPin,
    /// Clone children (global ids), killed at the end if still there.
    kids: alloc::vec::Vec<u32>,
}

impl Drop for Held {
    fn drop(&mut self) {
        for &k in &self.kids {
            if let Some(c) = proc::find(k)
                && !c.has_exited()
            {
                c.kill(9);
            }
        }
        self.p.kill(9);
        let _ = self.p.wait(Some(10 * SEC));
    }
}

impl Held {
    fn new(name: &str, uid: u32, program: ProgramId) -> Result<Held, String> {
        let image = crate::ktests::elf(&CODE);
        let argv = [name];
        let mut o = proc::SpawnOptions::new(&argv, &[], program);
        o.held = true;
        o.uid = uid;
        let p = proc::spawn_image_with(name, &image, &o).map_err(|e| format!("spawn {name}: {e}"))?;
        let mem = p.mm.sys_mmap(0, SCRATCH, 3, 0x22, 0).map_err(|e| format!("mmap: {e:?}"))?;
        let th = p.threads().into_iter().next().ok_or("no main thread")?;
        let _pin = proc::pin_mm(&p);
        Ok(Held { p, th, ctx: UserContext::new(ENTRY, mem + SCRATCH - 4096), mem, _pin, kids: alloc::vec::Vec::new() })
    }
    fn call(&mut self, n: usize, a: [u64; 4]) -> i64 {
        let r = &mut self.ctx.regs;
        (r.rax, r.rdi, r.rsi, r.rdx, r.r10) = (n as u64, a[0], a[1], a[2], a[3]);
        crate::dispatch(&self.p, &self.th, &mut self.ctx);
        self.ctx.regs.rax as i64
    }
    fn reboot(&mut self, m1: u32, m2: u32, cmd: u32) -> i64 {
        self.reboot_arg(m1, m2, cmd, 0)
    }
    fn reboot_arg(&mut self, m1: u32, m2: u32, cmd: u32, arg: u64) -> i64 {
        self.call(NR, [m1 as u64, m2 as u64, cmd as u64, arg])
    }
    /// clone3 (a new process, SIGCHLD to us) whose child runs the sleeper loop.
    fn clone_sleeper(&mut self) -> i64 {
        let mut b = [0u8; 88];
        b[32..40].copy_from_slice(&17u64.to_le_bytes());
        let _ = self.p.mm.copy_to_user(self.mem + 0x200, &b);
        self.ctx.regs.rip = SLEEP_AT;
        let r = self.call(uapi::nr::CLONE3, [self.mem + 0x200, 88, 0, 0]);
        self.ctx.regs.rip = ENTRY;
        if r > 0 {
            self.kids.push(r as u32);
        }
        r
    }
}

fn until(secs: u64, f: impl Fn() -> bool) -> bool {
    let end = time::now_ns() + secs * SEC;
    while time::now_ns() < end {
        if f() {
            return true;
        }
        sched::sleep_ns(10_000_000);
    }
    f()
}

ktest!("ul.reboot.errors", Safe, errors);
fn errors() -> TestResult {
    use reboot::*;
    const BOGUS: u32 = 0x1234_5678;
    let mut user = Held::new("ul7-user", 1000, ProgramId::SYSTEM)?;
    kassert!(user.reboot(MAGIC1, MAGIC2, BOGUS) == -EPERM, "uid 1000: EPERM before the magic and the command");
    kassert!(user.reboot(0, 0, BOGUS) == -EPERM, "uid 1000 with bad magic: still EPERM first");
    let mut root = Held::new("ul7-root", 0, ProgramId::SYSTEM)?;
    kassert!(root.reboot(MAGIC1 ^ 1, MAGIC2, BOGUS) == -EINVAL, "bad magic1: EINVAL");
    kassert!(root.reboot(MAGIC1, MAGIC2 ^ 1, BOGUS) == -EINVAL, "bad magic2: EINVAL");
    kassert!(root.reboot(MAGIC1, MAGIC2, BOGUS) == -EINVAL, "unknown command: EINVAL");
    kassert!(root.reboot(MAGIC1, MAGIC2, CMD_KEXEC) == -EINVAL, "KEXEC without an image: EINVAL");
    kassert!(root.reboot(MAGIC1, MAGIC2, CMD_SW_SUSPEND) == -EINVAL, "SW_SUSPEND without hibernation: EINVAL");
    let was = cad_on();
    let r = root.reboot(MAGIC1, MAGIC2A, CMD_CAD_OFF);
    let off = !cad_on();
    let r2 = root.reboot(MAGIC1, MAGIC2B, CMD_CAD_ON);
    let on = cad_on();
    let r3 = root.reboot(MAGIC1, MAGIC2C, if was { CMD_CAD_ON } else { CMD_CAD_OFF });
    kassert!(r == 0 && off && r2 == 0 && on && r3 == 0 && cad_on() == was, "CAD_OFF/CAD_ON set the flag ({r} {off} {r2} {on} {r3})");
    // A process of MCP's run is root here: CAD is not a power change, so it is allowed.
    let mut mcp = Held::new("ul7-mcp", 0, ProgramId(1))?;
    kassert!(mcp.reboot(MAGIC1, MAGIC2, if was { CMD_CAD_ON } else { CMD_CAD_OFF }) == 0, "CAD from MCP's run");
    Ok(())
}

fn push(code: u16, injected: bool) {
    let sink = console::evdev::sink();
    for value in [1, 0] {
        sink.push(InputEvent { time_ns: time::now_ns(), kind: 1, code, value, device: 0, injected });
        sink.push(InputEvent { time_ns: time::now_ns(), kind: 0, code: 0, value: 0, device: 0, injected });
    }
}

fn proc_power() -> String {
    let ns = vfs::root_ns();
    let c = vfs::Ctx::kernel(&ns);
    let Ok(f) = c.open(None, b"/proc/bitsos/power", vfs::abi::O_RDONLY, 0) else { return String::from("(no /proc/bitsos/power)") };
    let mut b = alloc::vec![0u8; 512];
    let n = f.read(&mut b).unwrap_or(0);
    String::from_utf8_lossy(&b[..n]).into_owned()
}

ktest!("ul.reboot.power_off_refused", QemuOnly, power_off_refused);
fn power_off_refused() -> TestResult {
    use reboot::*;
    const KEY_A: u16 = 30;
    const KEY_ENTER: u16 = 28;
    let mut mcp = Held::new("ul7-mcp-off", 0, ProgramId(1))?;
    push(KEY_ENTER, false);
    kassert!(mcp.reboot(MAGIC1, MAGIC2, CMD_POWER_OFF) == -EPERM, "POWER_OFF from MCP's run: EPERM even after a real Enter");
    kassert!(mcp.reboot(MAGIC1, MAGIC2, CMD_HALT) == -EPERM, "HALT from MCP's run: EPERM");
    let mut root = Held::new("ul7-root-off", 0, ProgramId::SYSTEM)?;
    push(KEY_A, false);
    push(KEY_ENTER, true);
    kassert!(input_verdict() == Err(Refusal::InjectedConfirm), "an injected Enter: {:?}", input_verdict());
    kassert!(root.reboot(MAGIC1, MAGIC2, CMD_POWER_OFF) == -EPERM, "POWER_OFF after an injected Enter: EPERM");
    let t = proc_power();
    kassert!(t.starts_with("poweroff=refused why=injected-confirm"), "/proc/bitsos/power after an injected Enter: {t}");
    push(KEY_A, true);
    push(KEY_ENTER, false);
    kassert!(input_verdict() == Err(Refusal::InjectedKeys), "injected keys ended by a real Enter: {:?}", input_verdict());
    kassert!(root.reboot(MAGIC1, MAGIC2, CMD_HALT) == -EPERM, "HALT after injected keys: EPERM");
    push(KEY_A, false);
    push(KEY_ENTER, false);
    kassert!(input_verdict().is_ok(), "a line typed on the machine allows: {:?}", input_verdict());
    let t = proc_power();
    kassert!(t.starts_with("poweroff=allowed"), "/proc/bitsos/power after a real line: {t}");
    // Leave the console with an injected line again, so nothing after this test finds a fresh real Enter.
    push(KEY_ENTER, true);
    Ok(())
}

ktest!("ul.reboot.restart2_arg", Safe, restart2_arg);
fn restart2_arg() -> TestResult {
    use reboot::restart2_arg as read;
    let h = Held::new("ul7-arg", 0, ProgramId::SYSTEM)?;
    let mm = &h.p.mm;
    // The scratch's last page goes, so a string can run into a hole.
    mm.sys_munmap(h.mem + SCRATCH - 4096, 4096).map_err(|e| format!("munmap: {e:?}"))?;
    let end = h.mem + SCRATCH - 4096;
    let put = |at: u64, b: &[u8]| mm.copy_to_user(at, b).map_err(|e| format!("put: {e:?}"));
    put(h.mem, b"firmware\0")?;
    kassert!(read(mm, h.mem) == Ok(String::from("firmware")), "a short string: {:?}", read(mm, h.mem));
    put(h.mem + 0x1000, &[b'x'; 300])?;
    let long = read(mm, h.mem + 0x1000);
    kassert!(long.as_ref().is_ok_and(|s| s.len() == 255 && s.bytes().all(|b| b == b'x')), "300 bytes without a NUL: 255 of them, no error ({:?})", long.map(|s| s.len()));
    put(end - 255, &[b'y'; 255])?;
    kassert!(read(mm, end - 255).is_ok_and(|s| s.len() == 255), "255 bytes up to the hole: no error");
    put(end - 100, &[b'z'; 100])?;
    kassert!(read(mm, end - 100) == Err(-EFAULT), "no NUL before the hole: EFAULT");
    put(end - 5, b"ok\0zz")?;
    kassert!(read(mm, end - 5) == Ok(String::from("ok")), "a NUL before the hole: no fault");
    kassert!(read(mm, end) == Err(-EFAULT), "the hole itself: EFAULT");
    kassert!(read(mm, 0xffff_8000_0000_0000) == Err(-EFAULT), "a kernel address: EFAULT");
    Ok(())
}

ktest!("ul.reboot.restart2_efault", QemuOnly, restart2_efault);
fn restart2_efault() -> TestResult {
    use reboot::*;
    let mut root = Held::new("ul7-r2", 0, ProgramId::SYSTEM)?;
    let hole = root.mem + SCRATCH - 4096;
    root.p.mm.sys_munmap(hole, 4096).map_err(|e| format!("munmap: {e:?}"))?;
    kassert!(root.reboot_arg(MAGIC1, MAGIC2, CMD_RESTART2, hole) == -EFAULT, "RESTART2 with an unmapped argument: EFAULT");
    kassert!(root.reboot_arg(MAGIC1, MAGIC2, CMD_RESTART2, 0xffff_8000_0000_0000) == -EFAULT, "RESTART2 with a kernel pointer: EFAULT");
    Ok(())
}

ktest!("ul.reboot.pid_ns", Safe, pid_ns);
fn pid_ns() -> TestResult {
    const NEWUSER_NEWPID: u64 = 0x1000_0000 | 0x2000_0000;
    const WNOHANG: u64 = 1;
    let mut t = Held::new("ul7-ns", 1000, ProgramId::SYSTEM)?;
    let r = t.call(uapi::nr::UNSHARE, [NEWUSER_NEWPID, 0, 0, 0]);
    kassert!(r == 0, "unshare(user, pid): {r}");
    let init = t.clone_sleeper();
    let second = t.clone_sleeper();
    kassert!(init > 0 && second > 0, "two children: {init} {second}");
    let ip = proc::find(init as u32).ok_or("the init is gone")?;
    kassert!(ip.is_ns_init() && !ip.pid_ns.is_init(), "the first child is a child namespace's init");
    kassert!(reboot::pid_ns_signal(reboot::decode(reboot::CMD_RESTART)) == Some(1), "RESTART ends a namespace with SIGHUP");
    kassert!(reboot::end_pid_ns(&ip.pid_ns, 1, t.p.pid), "the namespace had an init");
    drop(ip);
    let status = t.mem + 0x300;
    let mut got = 0;
    let end = time::now_ns() + 10 * SEC;
    while time::now_ns() < end {
        got = t.call(uapi::nr::WAIT4, [init as u64, status, WNOHANG, 0]);
        if got != 0 {
            break;
        }
        sched::sleep_ns(10_000_000);
    }
    kassert!(got == init, "wait4 of the namespace's init: {got}");
    let mut b = [0u8; 4];
    t.p.mm.copy_from_user(&mut b, status).map_err(|e| format!("status: {e:?}"))?;
    let st = u32::from_le_bytes(b);
    kassert!(st & 0x7f == 1, "the init ended by SIGHUP (WTERMSIG), status {st:#x}");
    kassert!(until(10, || proc::find(second as u32).is_none_or(|p| p.has_exited())), "the other member outlived its namespace");
    Ok(())
}
