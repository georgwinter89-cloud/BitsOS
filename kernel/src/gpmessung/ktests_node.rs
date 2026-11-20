//! The shell's way in (`/dev/bitsos/gpu-messung`, `/bin/gpu-messung`): QEMU has no RX 9070 XT, so the node's own
//! card answers the one German line; the question and its guard run on a machine that touches nothing (`Fake`),
//! through the node's own request (write `start`, read the answer) and the real sink and VT thread: MCP's
//! injected keys and a virtual keyboard never answer and type nothing into the console's line, a second start
//! (node or monitor) is refused while the question stands, a real "other key" ends it with nothing done, a real J
//! runs it, and after the question the console's line takes keys again. The program in the image reaches the
//! node; one MCP's run started is refused at the open.

use super::node::{OPEN_REFUSED, PATH, Request, answer};
use super::{BUSY, DECLINED, End, Hold, Machine, NOT_OFF};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use console::evdev::{self, Caps, DeviceInfo, EV_KEY, EV_SYN, InputEvent, SYN_REPORT};
use console::{monitor, report_page, text_ask, vt};
use core::sync::atomic::{AtomicU32, Ordering};
use gp_report::ask::KEY_JA;
use ktest::{TestResult, kassert, ktest};
use vfs::File;

const SEC: u64 = 1_000_000_000;
/// KEY_A: "any other key".
const KEY_A: u16 = 30;

/// Runs of the fake machine (each is a J that started it).
static RAN: AtomicU32 = AtomicU32::new(0);

struct Fake;

impl Machine for Fake {
    fn here(&mut self) -> Result<(), &'static str> {
        Ok(())
    }
    fn run(&mut self) -> Result<(), String> {
        RAN.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn fake() -> Box<dyn Machine> {
    Box::new(Fake)
}

fn text(e: &End) -> String {
    answer(e)
}

fn line(ran: bool, l: &str) -> String {
    text(&End { ran, line: String::from(l) })
}

fn read_all(f: &dyn File) -> Result<String, String> {
    let mut buf = alloc::vec![0u8; 4096];
    let n = f.read_stream(&mut buf, false).map_err(|e| format!("read the answer: errno {e}"))?;
    Ok(String::from_utf8_lossy(&buf[..n]).into_owned())
}

fn open_node() -> Result<Arc<vfs::OpenFile>, String> {
    let ns = vfs::root_ns();
    vfs::Ctx::kernel(&ns).open(None, PATH.as_bytes(), vfs::abi::O_RDWR, 0).map_err(|e| format!("open {PATH}: errno {e}"))
}

/// The node's own card on QEMU: the one German line, exit code 1, nothing of the card logged; one start per open,
/// only `start` asks.
fn node_no_card() -> TestResult {
    let since = klog::next_seq();
    let f = open_node()?;
    kassert!(f.write(b"anders\n").is_err(), "a write other than start was taken");
    f.write(b"start\n").map_err(|e| format!("write start: errno {e}"))?;
    let mut buf = alloc::vec![0u8; 4096];
    let n = f.read(&mut buf).map_err(|e| format!("read: errno {e}"))?;
    let got = String::from_utf8_lossy(&buf[..n]).into_owned();
    kassert!(got == line(false, amdgpu::rdna4::messlive::NONE_HERE), "without the card the node answered {got:?}");
    kassert!(f.read(&mut buf) == Ok(0), "a second read did not end the answer");
    kassert!(f.write(b"start\n").is_err(), "a second start on the same open file was taken");
    let page = klog::read(since, 200, &klog::Filter { level: None, subsystem: Some("gx"), name: Some("rdna4.") });
    kassert!(page.records.is_empty(), "a machine without the card logged the card's events: {:?}", page.records.first().map(|r| r.render()));
    kassert!(Hold::take().is_some(), "the hold stayed taken after the start ended");
    Ok(())
}
ktest!("gx.messung.node_no_card", QemuOnly, node_no_card);

fn kbd(name: &str, bus: u16, phys: &str) -> Arc<evdev::InputDevice> {
    let mut caps = Caps::default();
    caps.keys.extend([KEY_JA, KEY_A]);
    evdev::register(evdev::sink(), DeviceInfo { name: name.to_string(), phys: phys.to_string(), bus, caps, ..DeviceInfo::default() })
}

fn tap(k: &evdev::InputDevice, code: u16) {
    k.emit(time::now_ns(), &[(EV_KEY, code, 1), (EV_SYN, SYN_REPORT, 0)]);
    k.emit(time::now_ns(), &[(EV_KEY, code, 0), (EV_SYN, SYN_REPORT, 0)]);
}

/// MCP's `send_keys` path: injected events straight into the sink.
fn inject(code: u16) {
    let s = evdev::sink();
    for v in [1, 0] {
        let t = time::now_ns();
        s.push(InputEvent { time_ns: t, kind: EV_KEY, code, value: v, device: 0xFFFF_FF00, injected: true });
        s.push(InputEvent { time_ns: t, kind: EV_SYN, code: SYN_REPORT, value: 0, device: 0xFFFF_FF00, injected: true });
    }
}

fn settle() -> TestResult {
    let end = time::now_ns() + 5 * SEC;
    while vt::input_pending() > 0 {
        kassert!(time::now_ns() < end, "the VT did not take the events within 5 s");
        sched::sleep_ns(5_000_000);
    }
    sched::sleep_ns(100_000_000);
    Ok(())
}

/// Writes `start` and waits until the question stands and its keys count.
fn ask(r: &Request) -> TestResult {
    r.write_at(0, b"start").map_err(|e| format!("write start: errno {e}"))?;
    let end = time::now_ns() + 5 * SEC;
    while !text_ask::standing() {
        kassert!(time::now_ns() < end, "the question did not come up within 5 s");
        sched::sleep_ns(10_000_000);
    }
    sched::sleep_ns(report_page::ASK_ARM_NS + 300_000_000);
    Ok(())
}

fn not_answered(r: &Request, what: &str) -> TestResult {
    let mut buf = [0u8; 256];
    kassert!(r.read_stream(&mut buf, true) == Err(uapi::errno::EAGAIN), "{what} ended the start");
    Ok(())
}

fn node_question() -> TestResult {
    let _console = console::ktest_lease::ConsoleLease::take()?;
    let usb = kbd("ktest messung usb kbd", evdev::BUS_USB, "ktest-usb/input2");
    let virt = kbd("ktest messung virtual kbd", evdev::BUS_VIRTUAL, "");
    let r = question_steps(&usb, &virt);
    text_ask::close();
    console::tty().flush();
    usb.unregister(time::now_ns());
    virt.unregister(time::now_ns());
    r
}
ktest!("gx.messung.node_question", QemuOnly, node_question);

fn question_steps(usb: &evdev::InputDevice, virt: &evdev::InputDevice) -> TestResult {
    let t = console::tty();
    let before = RAN.load(Ordering::SeqCst);
    let first = Request::new(fake);
    ask(&first)?;
    // A second start while the question stands: the node and the monitor refuse it at once.
    let f = open_node()?;
    f.write(b"start").map_err(|e| format!("second start: errno {e}"))?;
    let got = read_all(&*f.file)?;
    kassert!(got == line(false, BUSY), "a second start through the node answered {got:?}");
    let (ok, out) = monitor::execute("gpu-messung", false, false);
    kassert!(ok && out == format!("{BUSY}\n"), "a second start through the monitor printed {out:?}");
    kassert!(text_ask::standing(), "the refused start took the first one's question down");

    let line_before = (t.editing(), t.inq());
    inject(KEY_JA);
    settle()?;
    not_answered(&first, "MCP's injected J")?;
    tap(virt, KEY_JA);
    tap(virt, KEY_A);
    settle()?;
    not_answered(&first, "a virtual keyboard")?;
    kassert!((t.editing(), t.inq()) == line_before, "keys while the question stood reached the console's line: {:?}", t.editing());
    tap(usb, KEY_A);
    let got = read_all(&first)?;
    kassert!(got == line(false, DECLINED), "a real other key answered {got:?}");
    kassert!(RAN.load(Ordering::SeqCst) == before, "the declined start ran the machine");
    kassert!((t.editing(), t.inq()) == line_before, "the answering key reached the console's line: {:?}", t.editing());

    // After the question the console's line takes keys again.
    tap(usb, KEY_A);
    settle()?;
    kassert!(t.editing() != line_before.0 || t.inq() != line_before.1, "a real key after the question did not reach the console's line");
    t.flush();

    let second = Request::new(fake);
    ask(&second)?;
    tap(usb, KEY_JA);
    let got = read_all(&second)?;
    kassert!(got == line(true, NOT_OFF), "the real J answered {got:?}");
    kassert!(RAN.load(Ordering::SeqCst) == before + 1, "the real J did not run the machine once");
    Ok(())
}

fn run_program(program: policy::ProgramId) -> Result<(Option<i32>, String), String> {
    let out = proc::stdio::Capture::new(1 << 16);
    let env = ["HOME=/", "PATH=/bin"];
    let mut o = proc::SpawnOptions::new(&["gpu-messung"], &env, program);
    o.stdout = Some(out.clone() as Arc<dyn File>);
    o.stderr = Some(out.clone() as Arc<dyn File>);
    o.uid = 0;
    let p = proc::spawn_with("/bin/gpu-messung", &o).map_err(|e| format!("spawn /bin/gpu-messung: {e}"))?;
    let code = p.wait(Some(20 * SEC));
    if code.is_none() {
        p.kill(uapi::ab::signal::SIGKILL);
        let _ = p.wait(Some(5 * SEC));
    }
    Ok((code, String::from_utf8_lossy(&out.contents()).into_owned()))
}

fn requests_since(since: u64) -> usize {
    klog::read(since, 200, &klog::Filter { level: None, subsystem: Some("gx"), name: Some("messung.request") }).records.len()
}

/// The image's /bin/gpu-messung reaches the node: on QEMU the one German line and exit code 1. Started by MCP's
/// run it is refused at the open and the node sees no request.
fn program_reaches_node() -> TestResult {
    kassert!(vfs::root_ns().lookup("/bin/gpu-messung").is_ok(), "no /bin/gpu-messung in the image");
    let since = klog::next_seq();
    let (code, out) = run_program(console_cmds::CONSOLE_PROGRAM)?;
    kassert!(code == Some(1), "gpu-messung ended with {code:?}: {out:?}");
    kassert!(out == format!("{}\n", amdgpu::rdna4::messlive::NONE_HERE), "gpu-messung printed {out:?}");
    kassert!(requests_since(since) == 1, "the node saw {} requests from the program", requests_since(since));

    let since = klog::next_seq();
    let refused = OPEN_REFUSED.load(Ordering::Relaxed);
    let (code, out) = run_program(proc::MCP_PROGRAM)?;
    kassert!(code.is_some_and(|c| c != 0), "under MCP's run gpu-messung ended with {code:?}: {out:?}");
    kassert!(OPEN_REFUSED.load(Ordering::Relaxed) > refused, "the node did not refuse MCP's run");
    kassert!(requests_since(since) == 0, "a program MCP's run started reached the node");
    let words: Vec<&str> = out.split_whitespace().collect();
    kassert!(words.contains(&"Fernsteuerung"), "the refusal under MCP is not the German line: {out:?}");
    Ok(())
}
ktest!("gx.messung.program", QemuOnly, program_reaches_node);
