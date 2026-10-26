//! SN-10 ktests (`sn.out.*`, Safe): `/sys/bitsos/snd_output` and `snd_output_status` through the file calls and the MCP tool `snd` over a
//! loopback server of the test's own (port 27560 or the next free one). Neither plays sound. With HDMI sound
//! possible (the Mini-PC) the `hdmi`/`hdmi-off` steps are left out (they write display registers: that is the
//! desk's check list, not a Safe test); without it (QEMU) they must be refused with the target unchanged. Both
//! put the target back as it was, so they rerun in one boot.

use crate::backend::{Port, Target};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use ktest::{TestResult, kassert, ktest};
use uapi::errno::{EACCES, EINVAL, ENODEV};

const PORT: u16 = 27560;

fn hdmi_possible() -> bool {
    hda::hdmi::gpu_controller().is_some() && hda::hdmi::display_state_now() != Some(hdacodec::dpaudio::State::Absent)
}

/// Runs `f` after the boot switch (`snd/boot-out`) is through, and puts the kernel mixer's target and the boot
/// switch's "a request came first" flag back after it, whatever `f` returned (SN-11 fixer: an early run over MCP
/// must not leave a stored hdmi unswitched, nor put auto back over the switch).
fn keeping_target(f: impl FnOnce(&Arc<crate::Mixer>) -> TestResult) -> TestResult {
    if crate::boot::settle(crate::boot::SETTLE_MS).is_none() {
        return Err(format!("snd/boot-out still runs after {} ms", crate::boot::SETTLE_MS));
    }
    let m = crate::mixer().map_err(|e| format!("mixer: {e:?}"))?;
    let old = m.target();
    let touched = crate::output::touched();
    let r = f(&m);
    m.set_target(old);
    crate::output::set_touched(touched);
    r
}

/// Every hda output as the status names it.
fn hda_outputs() -> alloc::vec::Vec<String> {
    let mut v = alloc::vec::Vec::new();
    for d in hda::controllers() {
        for o in d.ctrl.outputs() {
            let port = if o.port == devmodel::audio::Output::Hdmi { "hdmi" } else { "jack" };
            v.push(format!("{}/{port}/{}:{:#x}", d.ctrl.name(), o.id.cad, o.id.pin));
        }
    }
    v
}

fn sysfs() -> TestResult {
    keeping_target(|m| {
        use vfs::abi::{O_CREAT, O_RDONLY, O_TRUNC, O_WRONLY};
        use vfs::inode::Cred;
        use vfs::ops::Ctx;
        let ns = vfs::root_ns();
        let subject = policy::Subject { program: policy::ProgramId(1), pid: 0x7fff_0000, uid: 0 };
        let root = Ctx { ns: &ns, root: ns.root_path(), cwd: ns.root_path(), cred: Cred { uid: 0, gid: 0 }, umask: 0o022, subject };
        let user = Ctx { ns: &ns, root: ns.root_path(), cwd: ns.root_path(), cred: Cred { uid: 1000, gid: 1000 }, umask: 0o022, subject: policy::Subject { uid: 1000, ..subject } };
        let path = b"/sys/bitsos/snd_output";
        let f = root.open(None, path, O_WRONLY | O_CREAT | O_TRUNC, 0o644).map_err(|e| format!("open for writing: {e}"))?;
        kassert!(f.write(b"jack\n") == Ok(5), "write jack");

        kassert!(m.target() == Target::Prefer(Port::Jack), "target {:?} after jack", m.target());
        let read = |c: &Ctx| -> Result<String, String> {
            let r = c.open(None, path, O_RDONLY, 0).map_err(|e| format!("open for reading: {e}"))?;
            let mut buf = [0u8; 256];
            let n = r.read(&mut buf).map_err(|e| format!("read: {e}"))?;
            Ok(String::from_utf8_lossy(&buf[..n]).into())
        };
        // One value per file (sysfs): what a read gives, a write takes back.
        let t = read(&user)?;
        kassert!(t == "jack\n", "uid 1000 read {t:?}");
        let st = |c: &Ctx| -> Result<String, String> {
            let r = c.open(None, b"/sys/bitsos/snd_output_status", O_RDONLY, 0).map_err(|e| format!("open status: {e}"))?;
            let mut buf = [0u8; 256];
            let n = r.read(&mut buf).map_err(|e| format!("read status: {e}"))?;
            Ok(String::from_utf8_lossy(&buf[..n]).into())
        };
        let t = st(&user)?;
        kassert!(t.starts_with("mixer=running playing=") && t.ends_with('\n') && t.contains(" hdmi=") && t.lines().count() == 1, "status {t:?}");
        let sw = root.open(None, b"/sys/bitsos/snd_output_status", O_WRONLY, 0).map_err(|e| format!("open status for writing: {e}"))?;
        kassert!(sw.write(b"auto\n") == Err(EACCES), "root wrote the read-only status");
        kassert!(user.open(None, b"/sys/bitsos/snd_output_status", O_WRONLY, 0).err() == Some(EACCES), "uid 1000 opened status for writing");
        kassert!(f.pwrite(0, b"bogus\n") == Err(EINVAL), "a bad value was taken");
        kassert!(f.pwrite(0, b"fixed:hda1/jack") == Err(EINVAL), "a fixed target was taken");
        // Linux's sysfs_kf_write ignores the position (INT-W11 made /sys/bitsos do the same).
        kassert!(f.pwrite(1, b"jack") == Ok(4), "a write at offset 1 was refused");
        kassert!(m.target() == Target::Prefer(Port::Jack), "target {:?} after jack at offset 1", m.target());
        // Hostile values: never a panic, never a change.
        let big = alloc::vec![b'a'; 4097];
        for (what, v) in [("white space", &b"  \t\n"[..]), ("4097 bytes", &big[..]), ("not UTF-8", &b"\xff\xfe"[..]), ("a NUL", &b"jack\0"[..]), ("two values", &b"jack\nusb\n"[..])] {
            kassert!(f.pwrite(0, v) == Err(EINVAL), "{what} was taken");
        }
        // Empty: 0 and no change, as Linux's sysfs_kf_write (INT-W11).
        let e = f.pwrite(0, b"");
        kassert!(e == Ok(0), "empty write: {e:?}");
        kassert!(m.target() == Target::Prefer(Port::Jack), "target {:?} after refusals", m.target());
        kassert!(user.open(None, path, O_WRONLY, 0).err() == Some(EACCES), "uid 1000 opened it for writing");
        if hdmi_possible() {
            klog::event!(Info, "ktest", "sn.out.hdmi_left_out"; "HDMI sound is possible here: hdmi and hdmi-off are the desk's check list");
        } else {
            kassert!(f.pwrite(0, b"hdmi\n") == Err(ENODEV), "hdmi without a display engine was not ENODEV");
            kassert!(f.pwrite(0, b"hdmi-off\n") == Err(ENODEV), "hdmi-off without a display engine was not ENODEV");
            kassert!(f.pwrite(0, b"hdmi off\n") == Err(ENODEV), "hdmi off (the monitor's spelling) was not ENODEV");
            kassert!(m.target() == Target::Prefer(Port::Jack), "target {:?} after the HDMI refusals", m.target());
        }
        kassert!(f.pwrite(0, b"usb") == Ok(3), "write usb (no newline)");
        kassert!(m.target() == Target::Prefer(Port::Usb), "target {:?} after usb", m.target());
        kassert!(f.pwrite(0, b"auto\n") == Ok(5), "write auto");
        kassert!(read(&root)? == "auto\n", "root read after auto");
        Ok(())
    })
}
ktest!("sn.out.sysfs", Safe, sysfs);

fn tool() -> TestResult {
    use mcp::testclient::with_server;
    keeping_target(|m| {
        with_server(PORT, mcp::TOOLS.entries(), |c| {
            let t = c.ok("snd", "{}")?;
            kassert!(t.contains("target=") && t.contains("\nmixer=running") && t.contains("\nhdmi display="), "status {t}");
            for o in hda_outputs() {
                kassert!(t.contains(&format!("output {o} ")), "status lacks output {o}: {t}");
            }
            let r = c.call("snd", r#"{"output":"bogus"}"#)?;
            kassert!(r.rpc_error == Some(-32602), "bogus: {:?} {}", r.rpc_error, r.text);
            let r = c.call("snd", r#"{"output":7}"#)?;
            kassert!(r.rpc_error == Some(-32602), "a number: {:?} {}", r.rpc_error, r.text);
            let t = c.ok("snd", r#"{"output":"jack"}"#)?;
            kassert!(t.starts_with("snd output jack\ntarget=jack "), "jack: {t}");
            kassert!(m.target() == Target::Prefer(Port::Jack), "target {:?} after jack", m.target());
            if !hdmi_possible() {
                for v in ["hdmi", "hdmi-off", "hdmi off"] {
                    let r = c.call("snd", &format!(r#"{{"output":"{v}"}}"#))?;
                    kassert!(r.error && r.rpc_error.is_none() && r.text.contains("HDMI audio: "), "{v}: {:?} {}", r.rpc_error, r.text);
                }
                kassert!(m.target() == Target::Prefer(Port::Jack), "target {:?} after the HDMI refusals", m.target());
            }
            let t = c.ok("snd", r#"{"output":" usb "}"#)?;
            kassert!(t.contains("\ntarget=usb "), "usb: {t}");
            let t = c.ok("snd", r#"{"output":"auto"}"#)?;
            kassert!(t.contains("\ntarget=auto "), "auto: {t}");
            Ok(())
        })
    })
}
ktest!("sn.out.tool", Safe, tool);

/// SN-11: the boot switch's step with a stored value of the test's own. Without HDMI sound (QEMU) a stored `hdmi`
/// stays auto with the reason logged, the target unchanged and nothing handed to the display registers. With it
/// (the Mini-PC) only what writes nothing: a stored `jack` does nothing, a stored `hdmi` after a request is left
/// to that request, and the monitor sense (reads only) is logged. "Written" counts display writes only: the endpoint
/// index selects of a state read (`hdmi_possible`'s `display_state_now`) are not (SN-13, desk-minipc-w13 FAIL 1).
fn boot() -> TestResult {
    use crate::boot::{Boot, switch};
    keeping_target(|m| {
        let hdmi = Target::Prefer(Port::Hdmi);
        let before = m.target();
        let (writes, selects) = (hda::hdmi::write_calls(), hda::hdmi::index_selects());
        let r = switch(Some(&Target::Prefer(Port::Jack)), "ktest", true);
        kassert!(r == Boot::Nothing, "stored jack: {r:?}");
        if hdmi_possible() {
            crate::output::touch();
            let r = switch(Some(&hdmi), "ktest", false);
            kassert!(r == Boot::Decided, "stored hdmi after a request: {r:?}");
            klog::event!(Info, "snd", "sn.out.boot_sense", sense = format!("{:?}", hda::hdmi::monitor()); "reads only");
        } else {
            let r = switch(Some(&hdmi), "ktest", true);
            kassert!(matches!(&r, Boot::Auto(why) if why == "no GPU HD Audio function" || why == "no display engine"), "stored hdmi without HDMI: {r:?}");
        }
        kassert!(m.target() == before, "target {:?}, was {before:?}", m.target());
        let (w, s) = (hda::hdmi::write_calls() - writes, hda::hdmi::index_selects() - selects);
        klog::event!(Info, "snd", "sn.out.boot_writes", display = w, index_selects = s);
        kassert!(w == 0, "{w} display registers written ({s} endpoint index selects, not counted)");
        Ok(())
    })
}
ktest!("sn.out.boot", Safe, boot);

/// SN-12 (desk-minipc-w12 follow-up 1): every output request path takes its locks in table order. The real
/// `snd.output` with the real `hda.hdmi` under it, as the boot switch takes them (`monitor`, reads only), and,
/// without HDMI sound (QEMU), `hdmi_on`'s refusal as `apply`/`set_output` reach it (it takes `hda.hdmi` before it
/// looks for the GPU function); with HDMI sound (the Mini-PC) `hdmi_on` would write display registers, so only
/// the read. Then `apply`, `set_output` and the boot switch themselves. No violation may be recorded on this thread.
fn order() -> TestResult {
    use crate::output::{APPLY, Request, apply};
    use ksync::lockdep;
    if !lockdep::enabled() {
        return ktest::skip("lock checker not built in");
    }
    keeping_target(|m| {
        let (r, n) = lockdep::expect_violations(|| {
            let _one = APPLY.lock();
            let sense = hda::hdmi::monitor();
            let on = if hda::hdmi::gpu_controller().is_none() { Some(hda::hdmi::hdmi_on()) } else { None };
            (sense, on)
        });
        kassert!(n == 0, "{n} lock violations under snd.output -> hda.hdmi: {:?}", lockdep::list());
        if let (_, Some(on)) = &r {
            kassert!(on.is_err(), "hdmi_on without a GPU HD Audio function: {on:?}");
        }
        let (res, n) = lockdep::expect_violations(|| {
            let a = apply(&Request::Target(Target::Prefer(Port::Jack)), "ktest").map(|_| ());
            let b = crate::set_output("auto");
            let c = crate::boot::switch(Some(&Target::Prefer(Port::Jack)), "ktest", true);
            (a, b, c)
        });
        kassert!(n == 0, "{n} lock violations on the request paths: {:?}", lockdep::list());
        kassert!(res.0.is_ok() && res.1.is_ok() && res.2 == crate::boot::Boot::Nothing, "paths: {res:?}");
        kassert!(m.target() == Target::Auto, "target {:?} after auto", m.target());
        // The file path: write(2) holds `vfs.file.pos` while the attribute stores, so the request runs in a helper
        // thread (not this one's expect_violations): count the real ones naming the sound classes.
        let ours = || -> u64 {
            let names = ["snd.output", "hda.hdmi"];
            lockdep::list().iter().filter(|v| names.contains(&v.held) || names.contains(&v.taking)).map(|v| v.count - v.injected).sum()
        };
        let before = ours();
        let ns = vfs::root_ns();
        let subject = policy::Subject { program: policy::ProgramId(1), pid: 0x7fff_0000, uid: 0 };
        let root = vfs::ops::Ctx { ns: &ns, root: ns.root_path(), cwd: ns.root_path(), cred: vfs::inode::Cred { uid: 0, gid: 0 }, umask: 0o022, subject };
        let f = root.open(None, b"/sys/bitsos/snd_output", vfs::abi::O_WRONLY, 0).map_err(|e| format!("open for writing: {e}"))?;
        kassert!(f.write(b"jack\n") == Ok(5), "write jack");
        kassert!(m.target() == Target::Prefer(Port::Jack), "target {:?} after the file's jack", m.target());
        if !hdmi_possible() {
            kassert!(f.write(b"hdmi\n") == Err(ENODEV), "hdmi without HDMI sound was not ENODEV");
        }
        kassert!(f.write(b"auto\n") == Ok(5), "write auto");
        let after = ours();
        kassert!(after == before, "{} lock violations naming snd.output or hda.hdmi on the file path: {:?}", after - before, lockdep::list());
        Ok(())
    })
}
ktest!("sn.out.order", Safe, order);

/// SN-12 fixer: every branch of the file request's helper, and the writer's answer to each. A request withdrawn
/// before it began must not run after (its `begin` says no), a late one runs on and its result is dropped with
/// the slot once the thread ends (no leak), a failed thread start runs nothing. No sound, no display registers.
fn helper() -> TestResult {
    use crate::helper::{Waited, run, run_with};
    use crate::output::waited_errno;
    use core::sync::atomic::{AtomicBool, AtomicU8, Ordering::Acquire, Ordering::Release};
    use uapi::errno::{EBUSY, EINTR, EIO, ENOMEM};
    let until = |c: &dyn Fn() -> bool| {
        let t = time::now_ns();
        while !c() {
            if time::now_ns().saturating_sub(t) > 5_000_000_000 {
                return false;
            }
            sched::sleep_ms(5);
        }
        true
    };
    let real = |n: &str, job: alloc::boxed::Box<dyn FnOnce() + Send>| sched::spawn(n, sched::Class::Normal, job).is_ok();
    // A request that waits for `gate` before it begins (another request holding snd.output) and says what begin said.
    let gated = |gate: &Arc<AtomicBool>, saw: &Arc<AtomicU8>| {
        let (g, s) = (gate.clone(), saw.clone());
        move |t: &crate::helper::Ticket| {
            let start = time::now_ns();
            while !g.load(Acquire) && time::now_ns().saturating_sub(start) < 5_000_000_000 {
                sched::sleep_ms(5);
            }
            let b = t.begin();
            s.store(if b { 1 } else { 2 }, Release);
            b.then_some(7u32)
        }
    };

    let w = run("sn/helper-done", 2_000_000_000, |t| t.begin().then_some(3u32));
    kassert!(w == Waited::Done(3), "done: {w:?}");

    for signal in [false, true] {
        let (gate, saw) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicU8::new(0)));
        let w = run_with(real, || signal, "sn/helper-wd", 20_000_000, gated(&gate, &saw));
        kassert!(w == Waited::Cancelled { signal }, "withdrawn (signal {signal}): {w:?}");
        gate.store(true, Release);
        kassert!(until(&|| saw.load(Acquire) != 0), "the withdrawn request never got to begin");
        kassert!(saw.load(Acquire) == 2, "begin said yes after the withdraw (signal {signal})");
        kassert!(until(&|| Arc::strong_count(&gate) == 1), "the helper kept its request");
    }

    let marker = Arc::new(());
    let m = marker.clone();
    let w = run("sn/helper-late", 20_000_000, move |t| {
        if !t.begin() {
            return None;
        }
        sched::sleep_ms(200);
        Some(m)
    });
    kassert!(w == Waited::Late, "late: {w:?}");
    kassert!(until(&|| Arc::strong_count(&marker) == 1), "the late result was not dropped");

    let w = run_with(|_: &str, _job| false, || false, "sn/helper-none", 1, |t: &crate::helper::Ticket| t.begin().then_some(0u32));
    kassert!(w == Waited::NoThread, "no thread: {w:?}");

    kassert!(waited_errno(Waited::Done(Ok(String::new()))) == Ok(()), "done answer");
    kassert!(waited_errno(Waited::NoThread) == Err(ENOMEM), "no thread answer");
    kassert!(waited_errno(Waited::Cancelled { signal: true }) == Err(EINTR), "signal answer");
    kassert!(waited_errno(Waited::Cancelled { signal: false }) == Err(EBUSY), "bound answer");
    kassert!(waited_errno(Waited::Late) == Err(EIO), "late answer");
    Ok(())
}
ktest!("sn.out.helper", Safe, helper);
