//! Ring-3 smoke test (V2-PLAN §9.1 point 7): a static musl C program and a Rust std program run against the
//! wave-A stubs and print their line.

use alloc::string::String;

const HELLO_RS: (&str, &str) = ("/bin/hello-rs", "hello from Rust std on BitsOS V2, sum=10");
const HELLO_C: (&str, &str) = ("/bin/hello", "hello from musl C on BitsOS V2");

fn run(path: &str, expect: &str) -> Result<(), String> {
    let before = console::captured().len();
    let p = proc::spawn(path, &[path], &["HOME=/", "PATH=/bin"], policy::ProgramId(1)).map_err(|e| alloc::format!("spawn {path}: {e:?}"))?;
    let code = p.wait(Some(10_000_000_000)).ok_or_else(|| alloc::format!("{path} did not end within 10 s"))?;
    let out = console::captured();
    let text = String::from_utf8_lossy(&out[before.min(out.len())..]).into_owned();
    if code != 0 {
        return Err(alloc::format!("{path} exit code {code}, output {text:?}"));
    }
    if !text.contains(expect) {
        return Err(alloc::format!("{path} printed {text:?}"));
    }
    Ok(())
}

fn hello_rs() -> ktest::TestResult {
    run(HELLO_RS.0, HELLO_RS.1)
}
ktest::ktest!("smoke.hello_rs", Safe, hello_rs);

fn hello_c() -> ktest::TestResult {
    run(HELLO_C.0, HELLO_C.1)
}
ktest::ktest!("smoke.hello_c", Safe, hello_c);

/// Normal (non-ktest) boot: run both programs once so the picture shows ring 3 working.
pub fn run_at_boot() {
    for (p, e) in [HELLO_C, HELLO_RS] {
        match run(p, e) {
            Ok(()) => klog::event!(Info, "smoke", "ok", program = p),
            Err(e) => klog::event!(Warn, "smoke", "failed", program = p, error = e),
        }
    }
}
