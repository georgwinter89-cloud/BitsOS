//! SN-10 host tests: the values `/sys/bitsos/snd_output` and the `snd` tool take, their errnos, the file's text
//! and the per-client rows.

use crate::backend::{Port, Target};
use crate::client::{self, State, UNITY};
use crate::mixer::Shared;
use crate::mixer::{Config, Status};
use crate::output::{Refusal, Request, Why, display_name, leave_bound_ns, mixer_word, on_controller, parse_request, status_text, target_name};
use alloc::string::String;
use alloc::sync::Arc;
use devmodel::audio::PcmFormat;
use hdacodec::dpaudio::State as DpState;
use uapi::errno::{EINVAL, EIO, ENODEV};

#[test]
fn values() {
    assert_eq!(parse_request("auto"), Some(Request::Target(Target::Auto)));
    assert_eq!(parse_request("jack"), Some(Request::Target(Target::Prefer(Port::Jack))));
    assert_eq!(parse_request("hdmi"), Some(Request::Target(Target::Prefer(Port::Hdmi))));
    assert_eq!(parse_request("usb"), Some(Request::Target(Target::Prefer(Port::Usb))));
    assert_eq!(parse_request("hdmi-off"), Some(Request::HdmiOff));
    assert_eq!(parse_request("hdmi off"), Some(Request::HdmiOff));
    // White space around (MCP text, a CR from a Windows editor); the attribute layer drops one newline already.
    assert_eq!(parse_request(" usb\r\n"), Some(Request::Target(Target::Prefer(Port::Usb))));
    for bad in ["", "HDMI", "hdmi_off", "hdmioff", "fixed:hda1/jack", "auto auto", "jack\0", "\u{fc}"] {
        assert_eq!(parse_request(bad), None, "{bad:?}");
    }
}

#[test]
fn names_round_trip() {
    for t in [Target::Auto, Target::Prefer(Port::Jack), Target::Prefer(Port::Hdmi), Target::Prefer(Port::Usb)] {
        assert_eq!(parse_request(&target_name(&t)), Some(Request::Target(t.clone())), "{t:?}");
    }
    let f = Target::Fixed { dev: "hda1".into(), port: Port::Jack };
    assert_eq!(target_name(&f), "fixed:hda1/jack");
    assert_eq!(parse_request(&target_name(&f)), None, "a fixed target is not writable");
}

#[test]
fn errnos() {
    let e = |why| Refusal { why, text: String::new() }.errno();
    assert_eq!(e(Why::BadValue), EINVAL);
    assert_eq!(e(Why::NoHdmi), ENODEV);
    assert_eq!(e(Why::Failed), EIO);
}

#[test]
fn sys_file_text() {
    // snd_output is the target word alone (one value per sysfs file, `cat f > f` works); the rest is in
    // snd_output_status.
    for t in [Target::Auto, Target::Prefer(Port::Jack), Target::Prefer(Port::Hdmi), Target::Prefer(Port::Usb)] {
        let w = target_name(&t);
        assert!(!w.contains(char::is_whitespace), "{w:?}");
        assert_eq!(parse_request(&alloc::format!("{w}\n")), Some(Request::Target(t)));
    }
    assert_eq!(status_text("running", Some("hda0/jack/0:0x15"), 2, "absent"), "mixer=running playing=hda0/jack/0:0x15 clients=2 hdmi=absent");
    assert_eq!(status_text("idle", None, 0, "off"), "mixer=idle playing=none clients=0 hdmi=off");
    assert_eq!(display_name(Some(DpState::On)), "on");
    assert_eq!(display_name(Some(DpState::Off)), "off");
    assert_eq!(display_name(Some(DpState::Absent)), "absent");
    assert_eq!(display_name(None), "busy");
}

#[test]
fn stopped_or_idle_mixer() {
    // After Quiesce (Reload) the global mixer is stopped; before the first client it is not started.
    assert_eq!(mixer_word(None), "idle");
    assert_eq!(mixer_word(Some(true)), "stopped");
    assert_eq!(mixer_word(Some(false)), "running");
    assert_eq!(status_text(mixer_word(Some(true)), None, 0, "busy"), "mixer=stopped playing=none clients=0 hdmi=busy");
    // In host tests nothing started the kernel mixer: the status readers answer without starting it.
    assert!(crate::started().is_none());
    assert_eq!(crate::output::mixer_state(), "idle");
    assert!(crate::started().is_none(), "a status reader started the mixer");
}

fn st(endpoint: Option<&str>, streams: usize) -> Status {
    Status { endpoint: endpoint.map(String::from), streams, clients: 1, mixed: 0, played: 0, hw_underruns: 0, switches: 0, opens: 0, open_failures: 0 }
}

#[test]
fn leaving_the_gpu_controller() {
    assert!(on_controller(&st(Some("hda1/hdmi/0:0x3"), 1), "hda1"));
    // A retiring output (after the switch) still counts; any retiring one, as the status does not name them.
    assert!(on_controller(&st(Some("hda0/jack/0:0x15"), 2), "hda1"));
    assert!(!on_controller(&st(Some("hda0/jack/0:0x15"), 1), "hda1"));
    assert!(!on_controller(&st(None, 0), "hda1"));
    // A name that only starts with the controller's is another controller.
    assert!(!on_controller(&st(Some("hda10/jack/0:0x15"), 1), "hda1"));
    // The bound follows CONFIG snd_latency_ms: 2 x (lead + drain margin + 4 periods).
    let c = Config::latency_ms(40);
    let want = 2 * (c.lead as u64 + c.drain_margin as u64 + 4 * c.period as u64) * 1_000_000_000 / 48_000;
    assert_eq!(leave_bound_ns(&c), want);
    assert!(leave_bound_ns(&Config::latency_ms(500)) > leave_bound_ns(&c));
}

#[test]
fn client_rows() {
    let sh = Arc::new(Shared::new());
    assert!(sh.client_infos().is_empty());
    let fmt = PcmFormat { rate: 44100, channels: 2, bits: 16 };
    let p = client::open(&sh, fmt, 4096, "alsa:pid 7").unwrap();
    p.set_volume([UNITY / 2, UNITY]);
    let rows = sh.client_infos();
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!((r.id, r.name.as_str(), r.fmt, r.state, r.volume, r.consumed, r.underruns), (p.id(), "alsa:pid 7", fmt, State::Prepared, [UNITY / 2, UNITY], 0, 0));
    drop(p);
    // The mixer's next step removes it; until then its row says closed.
    assert!(sh.client_infos().iter().all(|r| r.state == State::Closed), "{:?}", sh.client_infos());
}

/// SN-11: the boot switch's decision. Only a stored `hdmi` does anything; a request that came first wins; without
/// the GPU function, the display engine or a lit monitor it stays auto and says why; a lit link switches.
#[test]
fn boot_decision() {
    use crate::boot::{Boot, decision};
    use hdacodec::dpaudio::{GATES, LINK_GATES, Sense};
    let hdmi = Target::Prefer(Port::Hdmi);
    for other in [None, Some(Target::Auto), Some(Target::Prefer(Port::Jack)), Some(Target::Prefer(Port::Usb))] {
        assert_eq!(decision(other.as_ref(), false, true, Sense::Lit), Boot::Nothing, "{other:?}");
    }
    assert_eq!(decision(Some(&hdmi), true, true, Sense::Lit), Boot::Decided);
    assert_eq!(decision(Some(&hdmi), false, false, Sense::Lit), Boot::Auto("no GPU HD Audio function".into()));
    assert_eq!(decision(Some(&hdmi), false, true, Sense::Absent), Boot::Auto("no display engine".into()));
    let g = GATES[LINK_GATES.start + 3];
    assert_eq!(g.name, "DP0 link trained");
    assert_eq!(decision(Some(&hdmi), false, true, Sense::Dark { gate: g, read: 0 }), Boot::Auto("no monitor lit: DP0 link trained (read 0)".into()));
    assert_eq!(decision(Some(&hdmi), false, true, Sense::Lit), Boot::Switch);
}

/// SN-11: the mixer starts auto for a stored `hdmi` (only the boot switch turns it on, with the display side),
/// with any other stored target as stored, and the status reader without a mixer says the same.
#[test]
fn boot_start_target() {
    use crate::thread::start_target;
    assert_eq!(start_target(Some(Target::Prefer(Port::Hdmi))), Target::Auto);
    assert_eq!(start_target(None), Target::Auto);
    assert_eq!(start_target(Some(Target::Prefer(Port::Jack))), Target::Prefer(Port::Jack));
    assert_eq!(start_target(Some(Target::Prefer(Port::Usb))), Target::Prefer(Port::Usb));
    assert!(crate::started().is_none());
    assert_eq!(crate::output::current_target(), Target::Auto);
}

/// SN-11 fixer: the switch step after the decision. `Switch` asks `apply` once for target hdmi with the caller's
/// source and stays `Switch` on Ok; a refusal turns into auto with its text; every other decision asks nothing.
#[test]
fn boot_act() {
    use crate::boot::{Boot, act};
    use core::cell::RefCell;
    let calls = RefCell::new(alloc::vec::Vec::new());
    let ok = |r: &Request, s: &'static str| {
        calls.borrow_mut().push((r.clone(), s));
        Ok(String::from("snd output hdmi"))
    };
    assert_eq!(act(Boot::Switch, "boot", ok), Boot::Switch);
    assert_eq!(*calls.borrow(), alloc::vec![(Request::Target(Target::Prefer(Port::Hdmi)), "boot")]);
    let refused = |_: &Request, _: &'static str| Err(Refusal { why: Why::Failed, text: "HDMI audio: link not trained".into() });
    assert_eq!(act(Boot::Switch, "boot", refused), Boot::Auto("HDMI audio: link not trained".into()));
    for d in [Boot::Nothing, Boot::Decided, Boot::Auto("no display engine".into())] {
        let never = |_: &Request, _: &'static str| -> Result<String, Refusal> { panic!("apply called for {d:?}") };
        assert_eq!(act(d.clone(), "boot", never), d);
    }
}

/// SN-11 fixer: the bounded waits of `snd/boot-out` on a fake clock (each sleep advances it): done at once is
/// Some(0) without a sleep, done later gives the time waited, never done ends after the bound with None.
#[test]
fn boot_wait() {
    use crate::boot::wait_until;
    use core::cell::Cell;
    let now = Cell::new(0u64);
    let sleeps = Cell::new(0u32);
    let clock = || now.get();
    let sleep = |ms: u64| {
        sleeps.set(sleeps.get() + 1);
        now.set(now.get() + ms * 1_000_000);
    };
    assert_eq!(wait_until(100, || true, clock, sleep), Some(0));
    assert_eq!(sleeps.get(), 0);
    let polls = Cell::new(0u32);
    assert_eq!(wait_until(1_000, || { polls.set(polls.get() + 1); polls.get() > 3 }, clock, sleep), Some(60));
    let start = now.get();
    assert_eq!(wait_until(100, || false, clock, sleep), None);
    assert!(now.get() - start >= 100_000_000 && now.get() - start < 130_000_000, "waited {} ns", now.get() - start);
}

/// SN-12 (desk-minipc-w12 follow-up 1): the lock sequence every output request takes, in table order. The boot
/// switch: `snd.output`, then `hda.hdmi` (`monitor`, then `hdmi_on` through `apply_locked`); sysfs, MCP and the
/// console: `snd.output`, the mixer (`snd.mixer`, start), `hda.hdmi` (`hdmi_on`/`hdmi_off`), the GPU function's
/// PCI chain (its restart), then the mixer's target and the scheduler (the make-before-break sleeps). The real
/// `output::APPLY` with stand-ins of the other classes (classes are by name).
#[test]
fn output_request_lock_order() {
    use ksync::{IrqSpinLock, Mutex, SpinLock, lockdep};
    if !lockdep::enabled() {
        return;
    }
    assert!(ksync::order::SND_OUTPUT < ksync::order::DRIVER_OUTER);
    let hdmi = Mutex::named("hda.hdmi", ());
    let mixer = Mutex::named("snd.mixer", ());
    let enable = Mutex::named("pci.enable", ());
    let cfg = SpinLock::named("pci.cfg", ());
    let target = SpinLock::named("snd.mix.target", ());
    let sched = IrqSpinLock::named("sched", ());
    let ((), n) = lockdep::expect_violations(|| {
        // The boot switch (boot::switch): the sense and the switch under one hold.
        let _one = crate::output::APPLY.lock();
        drop(hdmi.lock());
        let _h = hdmi.lock();
        let _e = enable.lock();
        drop(cfg.lock());
    });
    assert_eq!(n, 0, "boot switch order");
    let ((), n) = lockdep::expect_violations(|| {
        // apply / set_output: mixer first, then HDMI on or off, then the target and a sleep.
        let _one = crate::output::APPLY.lock();
        drop(mixer.lock());
        {
            let _h = hdmi.lock();
            let _e = enable.lock();
            drop(cfg.lock());
        }
        drop(target.lock());
        drop(sched.lock());
    });
    assert_eq!(n, 0, "apply order");
    // The table really orders them: snd.output under hda.hdmi or under a subsystem lock is reported.
    let ((), n) = lockdep::expect_violations(|| {
        let _h = hdmi.lock();
        drop(crate::output::APPLY.lock());
    });
    assert_eq!(n, 1, "snd.output under hda.hdmi");
    let ((), n) = lockdep::expect_violations(|| {
        let _m = mixer.lock();
        drop(crate::output::APPLY.lock());
    });
    assert_eq!(n, 1, "snd.output under snd.mixer");
}
