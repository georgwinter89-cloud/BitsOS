//! SELFOPT-1 program tests: `bitsprof` profiles `profbusy` (a program with one hot function) and its report names
//! that function (`so.perf.bitsprof`, the scenario host/tests/so/10-profile.sh; on the Mini-PC like `Safe`), and
//! the profiler's cost on a CPU-bound program (`so.perf.overhead`: times are judged on the Mini-PC only).

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};

const SEC: u64 = 1_000_000_000;
const DIR: &str = "/tmp/so-perf-prof";

fn have(path: &str) -> bool {
    vfs::root_ns().lookup(path).is_ok()
}

/// Starts `argv` as `uid` with its output captured.
fn start(argv: &[&str], uid: u32) -> Result<(Arc<proc::Process>, Arc<proc::stdio::Capture>), String> {
    let out = proc::stdio::Capture::new(1 << 16);
    let env = ["HOME=/", "PATH=/bin"];
    let mut o = proc::SpawnOptions::new(argv, &env, policy::ProgramId(1));
    o.uid = uid;
    o.stdin = Some(proc::stdio::Input::new(Vec::new()) as Arc<dyn vfs::File>);
    o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    o.stderr = Some(out.clone() as Arc<dyn vfs::File>);
    let p = proc::spawn_with(argv[0], &o).map_err(|e| format!("spawn {}: {e}", argv[0]))?;
    Ok((p, out))
}

fn finish(name: &str, p: &proc::Process, out: &proc::stdio::Capture, secs: u64) -> Result<String, String> {
    let code = match p.wait(Some(secs * SEC)) {
        Some(c) => c,
        None => {
            p.kill(9);
            let _ = p.wait(Some(10 * SEC));
            return Err(format!("{name} did not end within {secs} s:\n{}", String::from_utf8_lossy(&out.contents())));
        }
    };
    let text = String::from_utf8_lossy(&out.contents()).into_owned();
    kassert!(code == 0, "{name} ended with {code}:\n{text}");
    Ok(text)
}

fn programs_present() -> TestResult {
    for p in ["/bin/bitsprof", "/bin/profbusy"] {
        if !have(p) {
            return Err(format!("{p} is not in this image (did the user programs fail to build?)"));
        }
    }
    Ok(())
}

ktest!("so.perf.bitsprof", RunHw("so/10-profile"), bitsprof);
fn bitsprof() -> TestResult {
    programs_present()?;
    // One profbusy before the profiler (found in /proc), one after it started (seen through COMM and MMAP2).
    let (early, early_out) = start(&["/bin/profbusy", "4"], 1000)?;
    let (prof, prof_out) = start(&["/bin/bitsprof", "run", "--out", DIR, "--seconds", "3"], 0)?;
    sched::sleep_ns(SEC / 2);
    let (late, late_out) = start(&["/bin/profbusy", "2"], 1000)?;
    finish("bitsprof run", &prof, &prof_out, 60)?;
    finish("profbusy (early)", &early, &early_out, 60)?;
    finish("profbusy (late)", &late, &late_out, 60)?;
    let (rep, rep_out) = start(&["/bin/bitsprof", "report", DIR, "--json", "--top", "5"], 0)?;
    let json = finish("bitsprof report --json", &rep, &rep_out, 60)?;
    let (txt, txt_out) = start(&["/bin/bitsprof", "report", DIR], 0)?;
    let text = finish("bitsprof report", &txt, &txt_out, 60)?;
    let meta = vfs::root_ns().read_file(&format!("{DIR}/meta.json"), 1 << 16).map_err(|e| format!("meta.json: {e}"))?;
    let meta = String::from_utf8_lossy(&meta).into_owned();
    for l in text.lines().take(12) {
        klog::event!(Info, "ktest", "so.perf.report", line = l);
    }
    kassert!(meta.contains("\"system\": \"BitsOS\"") && meta.contains("\"kernel_symbols\": true"), "meta.json:\n{meta}");
    let prog = json.find("\"name\": \"profbusy\"").ok_or_else(|| format!("profbusy is not among the programs:\n{json}"))?;
    let rest = &json[prog..];
    let hot = rest.find("profbusy_hot_loop").ok_or_else(|| format!("profbusy's hot function is not named:\n{json}"))?;
    kassert!(rest[hot..].starts_with("profbusy_hot_loop\", \"module\": \"/bin/profbusy\""), "the hot function's module:\n{}", &rest[hot..rest.len().min(hot + 120)]);
    kassert!(text.contains("profbusy_hot_loop") && text.contains("Programme:"), "the text report:\n{text}");
    kassert!(json.contains("\"kernel\": {\"samples\": ") && !json.contains("\"function\": \"[Kern]\""), "kernel frames without names:\n{json}");
    // The runner's way: no --seconds, stopped by SIGTERM; both files are written and the exit is 0.
    let dir = format!("{DIR}-term");
    let (prof, prof_out) = start(&["/bin/bitsprof", "run", "--out", &dir], 0)?;
    sched::sleep_ns(SEC);
    // A real SIGTERM (Process::kill ends a process at once, like SIGKILL).
    kassert!(proc::deliver::kill_pgrp(prof.pgid(), 15) == 1, "SIGTERM did not reach bitsprof");
    finish("bitsprof run (SIGTERM)", &prof, &prof_out, 10)?;
    for f in ["profile.bin", "meta.json"] {
        kassert!(vfs::root_ns().lookup(&format!("{dir}/{f}")).is_ok(), "{f} missing after SIGTERM");
    }
    Ok(())
}

/// `rate=` of a profbusy run.
fn rate(out: &str) -> Option<f64> {
    out.split_whitespace().find_map(|w| w.strip_prefix("rate=")).and_then(|r| r.parse().ok())
}

fn busy_rate(secs: &str, threads: &str) -> Result<f64, String> {
    let (p, out) = start(&["/bin/profbusy", secs, threads], 1000)?;
    let text = finish("profbusy", &p, &out, 120)?;
    rate(&text).ok_or_else(|| format!("profbusy printed no rate: {text}"))
}

// The profiler's cost on a CPU-bound program: profbusy on every CPU, five runs without and five with bitsprof at
// 1 kHz; the best run of each (the least disturbed) is compared. Plus the kernel's own sampling time per CPU.
ktest!("so.perf.overhead", Hardware, overhead);
fn overhead() -> TestResult {
    programs_present()?;
    let threads = format!("{}", hal::percpu::cpu_count());
    let (mut without, mut with) = (0f64, 0f64);
    for _ in 0..5 {
        without = without.max(busy_rate("3", &threads)?);
    }
    let (t0, ns0) = super::sampler::cost();
    for i in 0..5 {
        let dir = format!("{DIR}-cost{i}");
        let (prof, prof_out) = start(&["/bin/bitsprof", "run", "--out", &dir, "--seconds", "4"], 0)?;
        sched::sleep_ns(SEC / 2);
        with = with.max(busy_rate("3", &threads)?);
        finish("bitsprof run", &prof, &prof_out, 60)?;
    }
    let (t1, ns1) = super::sampler::cost();
    let loss = if without > 0.0 { (without - with) / without * 100.0 } else { 100.0 };
    let irq_ns = (ns1 - ns0) / (t1 - t0).max(1);
    klog::event!(Info, "ktest", "so.perf.overhead", without = without as u64, with = with as u64, loss_permille = (loss * 10.0) as i64, irq_ns_per_sample = irq_ns);
    kassert!(loss < 1.5, "profbusy lost {loss:.2} % of its work with the profiler (target about 1 %)");
    Ok(())
}
