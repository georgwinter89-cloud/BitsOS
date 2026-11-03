//! GX-7m: the VM stage on the host (the ktests' checks, `ktests_vm`) and its answer lines.

use crate::gpucore::fakechip::{self, StartAs};
use crate::ktests_vm as k;

#[test]
fn runs_sizes_and_after_linux() {
    k::check_runs(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn failures_tear_down() {
    k::check_failures(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn negative_cases() {
    k::check_negative(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn stalled_queues_come_back() {
    k::check_stall(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn a_failed_recovery_leaves_no_double_remove() {
    k::check_left(&fakechip::discovery_780m()).unwrap();
    k::check_left_proc(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn packets_as_linux() {
    k::check_packets(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn ib_pages_executable_and_fetch_needs_it() {
    k::check_exec(&fakechip::discovery_780m()).unwrap();
}

#[test]
fn hub_fake_walks_latches_and_faults() {
    k::check_hub(&fakechip::discovery_780m()).unwrap();
}

/// The answer: the fault line the desk greps for, the verdict last.
#[test]
fn answer_lines() {
    let d = fakechip::discovery_780m();
    let (out, _) = k::vm_run(&d, 512 << 20, StartAs::Cold, &|_| {}).unwrap();
    let text = crate::gpucore::vmring::render(out.vm.as_ref().unwrap());
    std::eprintln!("{text:#?}");
    for want in ["vm built: VMID 1", "vm mapping: VA 0x100000000", "vm GFX (MES) VM flush in the ring: ok", "gfx vm fault: GFX ring 0 (MES)", "gfx vm fault: SDMA 0", "vm MES process queue ADD_QUEUE with the VM and ring test: ok", "gfx vm fault: MES process queue (compute)", "vm process queue: compute, doorbell 0x1a", "vm: VMID 1 stays mapped, the MES process queue with it", "vm verdict (stage): PASS"] {
        assert!(text.iter().any(|l| l.starts_with(want)), "no line {want:?} in {text:#?}");
    }
}

/// The tool's `until=vm`: parsed, refused unless it continues a kept passed gfx_mes, its last line.
#[test]
fn tool_args_preflight_and_verdict() {
    use crate::gpucore::psp::Until;
    use crate::psplive::{self, Action, Args, Pre, preflight};
    let a = |t: &str| psplive::parse_args(&mcp::Json::parse(t).unwrap());
    assert_eq!(a(r#"{"until":"vm","keep":true}"#), Ok(Args { action: Action::Run(Until::Vm), keep: true, again: false, variant: None, ack_linux: true }));
    assert_eq!(a(r#"{"until":"vm","ack":"linux"}"#).map(|x| x.ack_linux), Ok(true));
    assert_eq!(a(r#"{"until":"vm","ack":"helper"}"#).map(|x| x.ack_linux), Ok(false));
    assert_eq!(a(r#"{"until":"gfx_mes","ack":"linux"}"#), Err(alloc::string::String::from("ack (only with until=vm)")));
    assert_eq!(a(r#"{"until":"vm","ack":"x"}"#), Err(alloc::string::String::from("ack (linux or helper)")));
    let fresh = Pre {
        reboot_needed: false,
        quarantined: false,
        display: Ok(()),
        has_bos: true,
        until: Until::Vm,
        m2_runs: 1,
        again: false,
        kept: false,
        gfx_runs: 1,
        gc_failed: false,
        reset: false,
        gmc_runs: 1,
        kept_gc_booted: false,
        sdma_runs: 1,
        gart_up: true,
        gfx_ring_runs: 1,
        mes_runs: 1,
        mes_up: true,
        mes_stranded: false,
        mes_queues: true,
    };
    let why = |p: &Pre| preflight(p).unwrap_err();
    assert!(why(&fresh).contains("no ring is kept now"), "{}", why(&fresh));
    let kept = Pre { kept: true, kept_gc_booted: true, ..fresh.clone() };
    assert_eq!(preflight(&kept), Ok(()), "after gfx_mes: no again needed (MES is not restarted)");
    assert!(why(&Pre { mes_queues: false, ..kept.clone() }).contains("until=gfx_mes with keep first"));
    assert!(why(&Pre { kept_gc_booted: false, ..kept.clone() }).contains("until=vm continues only"));
    assert!(why(&Pre { mes_stranded: true, ..kept.clone() }).contains("STRANDED"));
    let u = Until::Vm;
    assert!(u.loads_fw() && u.runs_gc() && u.runs_gmc() && u.after_gart() && !u.runs_mes() && !u.runs_gfx_stage() && u.maps_queues() && u.fake_chain_mes() && u.fake_chain_sdma());
    assert_eq!(psplive::vm_verdict_line(true), "vm verdict (vm): PASS");
    assert_eq!(psplive::vm_verdict_line(false), "vm verdict (vm): FAIL");
}

