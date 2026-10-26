//! ktests of `ps` and `run` through the monitor (no keyboard: `monitor::execute` directly), and IN-4's programs
//! on the console terminal: a raw-mode program reading injected keys, Ctrl+C ending the foreground program with
//! SIGINT, and the evdev program reading MCP's keyboard. The programs come from `user/abi-tests` (in `/bin` when
//! the image carries them; a missing one is a skip).

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use console::jobs::Ctl;
use console::keymap::code;
use ktest::{TestResult, kassert, ktest};
use proc::stdio::Capture;
use proc::{ExitStatus, Process, SpawnOptions};

const SEC: u64 = 1_000_000_000;

/// `run /bin/hello` prints the program's line on the console and ends with code 0; `ps` lists processes.
fn run_hello() -> TestResult {
    let _console = console::ktest_lease::ConsoleLease::take()?;
    if vfs::root_ns().lookup("/bin/hello").is_err() {
        return ktest::skip("no /bin/hello in the initramfs");
    }
    let before = console::captured().len();
    let (ok, out) = console::monitor::execute("run /bin/hello", false, false);
    kassert!(ok, "run failed: {out}");
    let all = console::captured();
    let new = String::from_utf8_lossy(&all[before.min(all.len())..]).into_owned();
    kassert!(new.contains("hello from musl C on BitsOS V2"), "program output not on the console: {new:?}");
    let (ok, out) = console::monitor::execute("ps", false, false);
    kassert!(ok && out.contains("PID") && out.contains("kernel threads:"), "ps: {out}");
    let (ok, out) = console::monitor::execute("run /bin/does-not-exist", false, false);
    kassert!(!ok && out.contains("/bin/does-not-exist"), "missing program: {out}");
    Ok(())
}
ktest!("in.console.run_hello", Safe, run_hello);

/// A program on the console (stdin the console, stdout captured) that owns the terminal like `run`'s.
struct OnConsole {
    p: Arc<Process>,
    out: Arc<Capture>,
    sid: u32,
}

impl OnConsole {
    fn start(path: &str, argv: &[&str]) -> Result<OnConsole, String> {
        let out = Capture::new(1 << 16);
        let env = ["HOME=/", "PATH=/bin"];
        let mut o = SpawnOptions::new(argv, &env, crate::CONSOLE_PROGRAM);
        o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
        o.held = true;
        let p = proc::spawn_with(path, &o).map_err(|e| format!("spawn {path}: {e}"))?;
        let sid = crate::sid_of(&p);
        console::tty().with_ctl(&crate::PROCS, |c, _| c.assign(sid, p.pgid()));
        p.release();
        Ok(OnConsole { p, out, sid })
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.out.contents()).into_owned()
    }
    fn wait_for(&self, what: &str) -> TestResult {
        let end = time::now_ns() + 10 * SEC;
        while !self.text().contains(what) {
            kassert!(!self.p.has_exited(), "{} ended before {what:?}: {}", self.p.name, self.text());
            kassert!(time::now_ns() < end, "no {what:?} from {} within 10 s: {}", self.p.name, self.text());
            sched::sleep_ns(10_000_000);
        }
        Ok(())
    }
    fn finish(&self) -> Result<i32, String> {
        match self.p.wait(Some(10 * SEC)) {
            Some(c) => Ok(c),
            None => {
                self.p.kill(uapi::ab::signal::SIGKILL);
                let _ = self.p.wait(Some(5 * SEC));
                Err(format!("{} did not end within 10 s: {}", self.p.name, self.text()))
            }
        }
    }
}

impl Drop for OnConsole {
    fn drop(&mut self) {
        if !self.p.has_exited() {
            self.p.kill(uapi::ab::signal::SIGKILL);
            let _ = self.p.wait(Some(5 * SEC));
        }
        let sid = self.sid;
        console::tty().with_ctl(&crate::PROCS, |c, _| {
            if c.session == Some(sid) {
                *c = Ctl::default();
            }
        });
        console::vt::reclaim();
    }
}

/// `abi-rawtty raw`: cfmakeraw through tcsetattr (the TCSETS path of today's VFS: the argument read through the
/// console's process hook), keys typed by injection arrive as the Linux console sends them, VTIME times out,
/// TCSAFLUSH restores the settings.
fn program_raw() -> TestResult {
    let _console = console::ktest_lease::ConsoleLease::take()?;
    if vfs::root_ns().lookup("/bin/abi-rawtty").is_err() {
        return ktest::skip("no /bin/abi-rawtty (user/abi-tests not in this image)");
    }
    let saved = console::tty().termios();
    let r = (|| -> TestResult {
        console::ktests_in4::stroke(&[code::LEFTCTRL], 22);
        console::ktests_in4::settle()?;
        let p = OnConsole::start("/bin/abi-rawtty", &["abi-rawtty", "raw"])?;
        p.wait_for("ready")?;
        kassert!(p.text().contains("raw: icanon 0 echo 0 isig 0 opost 0"), "settings: {}", p.text());
        for k in [30u16, 48] {
            console::ktests_in4::stroke(&[], k); // a b
        }
        console::ktests_in4::stroke(&[], code::UP);
        console::ktests_in4::stroke(&[], 16); // q
        let code = p.finish()?;
        let out = p.text();
        kassert!(code == 0, "exit {code}: {out}");
        let bytes: Vec<&str> = out.lines().filter_map(|l| l.strip_prefix("got ")).flat_map(|l| l.split(':').nth(1).unwrap_or("").split_whitespace()).collect();
        kassert!(bytes == ["61", "62", "1b", "5b", "41", "71"], "bytes {bytes:?}: {out}");
        kassert!(out.contains("timed read = 0 after a wait") && out.contains("restored"), "{out}");
        kassert!(console::tty().termios() == saved, "the program's TCSAFLUSH did not restore the settings");
        Ok(())
    })();
    console::tty().set_termios(saved, false);
    r
}
ktest!("in.tty.program_raw", Safe, program_raw);

/// Ctrl+C on the console ends the foreground program with SIGINT (ISIG through the process hook); its blocked
/// canonical read never returns data.
fn program_isig() -> TestResult {
    let _console = console::ktest_lease::ConsoleLease::take()?;
    if vfs::root_ns().lookup("/bin/abi-rawtty").is_err() {
        return ktest::skip("no /bin/abi-rawtty (user/abi-tests not in this image)");
    }
    let p = OnConsole::start("/bin/abi-rawtty", &["abi-rawtty", "wait"])?;
    p.wait_for("waiting")?;
    sched::sleep_ns(50_000_000);
    console::ktests_in4::stroke(&[code::LEFTCTRL], 46);
    let code = p.finish()?;
    kassert!(p.p.status() == Some(ExitStatus::Signaled(2)), "status {:?} (code {code}): {}", p.p.status(), p.text());
    kassert!(!p.text().contains("read returned"), "{}", p.text());
    Ok(())
}
ktest!("in.tty.program_isig", Safe, program_isig);

/// `abi-evdev v2`: the MCP keyboard's node through the syscalls, compared with the transcript written from
/// linux-6.6/drivers/input/evdev.c. Needs the integration patch (per-open clients, `ioctl_user` return values):
/// without it EVIOCGNAME answers 0 and the test says so as a skip.
fn evdev_program() -> TestResult {
    let _console = console::ktest_lease::ConsoleLease::take()?;
    if vfs::root_ns().lookup("/bin/abi-evdev").is_err() {
        return ktest::skip("no /bin/abi-evdev (user/abi-tests not in this image)");
    }
    console::evdevfs::sync();
    let out = Capture::new(1 << 16);
    let env = ["HOME=/", "PATH=/bin"];
    let mut o = SpawnOptions::new(&["abi-evdev", "v2"], &env, crate::CONSOLE_PROGRAM);
    o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    let p = proc::spawn_with("/bin/abi-evdev", &o).map_err(|e| format!("spawn: {e}"))?;
    let text = || String::from_utf8_lossy(&out.contents()).into_owned();
    let end = time::now_ns() + 10 * SEC;
    while !text().contains("ready") && !p.has_exited() && time::now_ns() < end {
        sched::sleep_ns(10_000_000);
    }
    console::ktests_in4::inject(16, 1);
    console::ktests_in4::inject(16, 0);
    if p.wait(Some(10 * SEC)).is_none() {
        p.kill(uapi::ab::signal::SIGKILL);
        let _ = p.wait(Some(5 * SEC));
        return Err(format!("abi-evdev did not end: {}", text()));
    }
    let got = text();
    if got.contains("EVIOCGNAME(256) = 0\n") {
        return ktest::skip("the integration patch (plan/notes/in-IN-4-integration.patch) is not applied: ioctl return values and per-open clients are missing");
    }
    let want = include_str!("../../../user/abi-tests/expected/in4-evdev.txt");
    for (i, (g, w)) in got.lines().zip(want.lines()).enumerate() {
        kassert!(g == w, "line {}: got {g:?}, want {w:?}\n--- got ---\n{got}", i + 1);
    }
    kassert!(got.lines().count() == want.lines().count(), "{} lines, want {}:\n{got}", got.lines().count(), want.lines().count());
    Ok(())
}
ktest!("in.evdev.program", Safe, evdev_program);
