//! VB-1 ktest: the report page's question takes only a real keyboard's key. Fake devices through the real sink
//! and VT thread: MCP's injected keys and a virtual-bus keyboard (as QEMU's or a program's would be) turn the
//! page and never answer; a keyboard registered like a USB one answers, but only once armed (its J before that
//! is taken: the page neither turns nor ends, and the rows say it came too early).

use crate::evdev::{self, Caps, DeviceInfo, EV_KEY, EV_SYN, InputEvent, SYN_REPORT};
use crate::{report_page, vt};
use alloc::string::{String, ToString};
use core::sync::atomic::{AtomicU8, Ordering};
use gp_report::ask::{Back, KEY_JA, State};
use ktest::{TestResult, kassert, ktest};

/// 0: no answer yet, 1: Ja, 2: Zurück.
static GOT: AtomicU8 = AtomicU8::new(0);

fn got(yes: bool) {
    GOT.store(if yes { 1 } else { 2 }, Ordering::Release);
}

fn kbd(name: &str, bus: u16, phys: &str) -> alloc::sync::Arc<evdev::InputDevice> {
    let mut caps = Caps::default();
    // KEY_A (30): any other key, for the start question.
    caps.keys.extend([KEY_JA, gp_report::ask::KEY_ZURUECK, 30]);
    evdev::register(evdev::sink(), DeviceInfo { name: name.to_string(), phys: phys.to_string(), bus, caps, ..DeviceInfo::default() })
}

fn tap(k: &evdev::InputDevice) {
    k.emit(time::now_ns(), &[(EV_KEY, KEY_JA, 1), (EV_SYN, SYN_REPORT, 0)]);
    k.emit(time::now_ns(), &[(EV_KEY, KEY_JA, 0), (EV_SYN, SYN_REPORT, 0)]);
}

fn inject() {
    let s = evdev::sink();
    for v in [1, 0] {
        let t = time::now_ns();
        s.push(InputEvent { time_ns: t, kind: EV_KEY, code: KEY_JA, value: v, device: 0xFFFF_FF00, injected: true });
        s.push(InputEvent { time_ns: t, kind: EV_SYN, code: SYN_REPORT, value: 0, device: 0xFFFF_FF00, injected: true });
    }
}

fn settle() -> Result<(), String> {
    let end = time::now_ns() + 5_000_000_000;
    while vt::input_pending() > 0 {
        kassert!(time::now_ns() < end, "the VT did not take the events within 5 s");
        sched::sleep_ns(5_000_000);
    }
    sched::sleep_ns(100_000_000);
    Ok(())
}

fn page() -> usize {
    report_page::shown().map_or(0, |p| p.0)
}

fn real_keys_only() -> TestResult {
    let _console = crate::ktest_lease::ConsoleLease::take()?;
    let pages = (1..=6).map(|i| alloc::vec![alloc::format!("Seite {i} von 6"), String::from("Version 168 läuft. Bestätigen?")]).collect();
    // In the gate's ktest boot an earlier test's program took the screen and ended the page for the boot.
    kassert!(report_page::reopen_for_ktest(), "a program holds the screen (graphics mode): no page can come up");
    kassert!(report_page::show(pages), "the page did not come up");
    GOT.store(0, Ordering::Release);
    report_page::set_ask(Some(State::asking(Back::To(167))), got);
    let usb = kbd("ktest ask usb kbd", evdev::BUS_USB, "ktest-usb/input0");
    let virt = kbd("ktest ask virtual kbd", evdev::BUS_VIRTUAL, "");
    let r = (|| -> TestResult {
        settle()?;
        tap(&usb);
        settle()?;
        kassert!(GOT.load(Ordering::Acquire) == 0, "a real J before the question was armed answered");
        kassert!(page() == 1, "the early J turned the page (page {})", page());
        kassert!(report_page::early(), "the early J was not noted for the rows");
        sched::sleep_ns(report_page::ASK_ARM_NS + 300_000_000);
        inject();
        settle()?;
        kassert!(GOT.load(Ordering::Acquire) == 0, "MCP's injected J answered");
        tap(&virt);
        settle()?;
        kassert!(GOT.load(Ordering::Acquire) == 0, "a virtual keyboard's J answered");
        kassert!(page() == 3, "injected and virtual keys did not turn the page (page {})", page());
        tap(&usb);
        settle()?;
        kassert!(GOT.load(Ordering::Acquire) == 1, "the real keyboard's J did not say Ja (got {})", GOT.load(Ordering::Acquire));
        kassert!(page() == 3, "the answering key turned the page too");
        Ok(())
    })();
    report_page::set_ask(None, got);
    report_page::end("ktest");
    vt::show_text();
    usb.unregister(time::now_ns());
    virt.unregister(time::now_ns());
    r
}
ktest!("gp.report.ask_real_keys", QemuOnly, real_keys_only);

/// The measurement run's start question on the text console: injected and virtual keys never answer, a real
/// key before it counts is taken and answers nothing, then J starts and any other real key says no.
fn start_question_real_keys_only() -> TestResult {
    use crate::text_ask::{self, Answer};
    let _console = crate::ktest_lease::ConsoleLease::take()?;
    let usb = kbd("ktest start usb kbd", evdev::BUS_USB, "ktest-usb/input1");
    let virt = kbd("ktest start virtual kbd", evdev::BUS_VIRTUAL, "");
    let other = |k: &evdev::InputDevice| {
        k.emit(time::now_ns(), &[(EV_KEY, 30, 1), (EV_SYN, SYN_REPORT, 0)]);
        k.emit(time::now_ns(), &[(EV_KEY, 30, 0), (EV_SYN, SYN_REPORT, 0)]);
    };
    let r = (|| -> TestResult {
        text_ask::open(&[String::from("ktest: Taste J startet")]);
        settle()?;
        tap(&usb);
        settle()?;
        kassert!(text_ask::poll().is_none(), "a real J before the question counted answered");
        sched::sleep_ns(report_page::ASK_ARM_NS + 300_000_000);
        inject();
        settle()?;
        kassert!(text_ask::poll().is_none(), "MCP's injected J answered");
        tap(&virt);
        other(&virt);
        settle()?;
        kassert!(text_ask::poll().is_none(), "a virtual keyboard answered");
        tap(&usb);
        settle()?;
        kassert!(text_ask::poll() == Some(Answer::Ja), "the real keyboard's J did not start (got {:?})", text_ask::poll());
        text_ask::close();
        text_ask::open(&[String::from("ktest: jede andere Taste bricht ab")]);
        sched::sleep_ns(report_page::ASK_ARM_NS + 300_000_000);
        other(&usb);
        settle()?;
        kassert!(text_ask::poll() == Some(Answer::Nein), "another real key did not say no (got {:?})", text_ask::poll());
        Ok(())
    })();
    text_ask::close();
    usb.unregister(time::now_ns());
    virt.unregister(time::now_ns());
    r
}
ktest!("console.start_question_real_keys", QemuOnly, start_question_real_keys_only);
