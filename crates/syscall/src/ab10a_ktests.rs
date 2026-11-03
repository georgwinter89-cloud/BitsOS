//! AB-10a ktests (`ab.ab10a.*`): the user selector layout, the int 0x80 TLS/LDT calls and the per-thread
//! segment state Wine's WoW64 needs. The `abi-wow64` program (user/abi-tests) runs in its ten modes against
//! CachyOS's transcripts (`expected/wow64-*.txt`); two in-kernel checks confirm the GDT layout and the
//! descriptor maths. The program exercises the end-to-end path (int 0x80 from 32-bit-ABI calls, a far return
//! that must fault, the hard-coded selectors, 32-bit code with CS 0x23 and signals in it, TLS/LDT state per
//! thread and per process across fork and exec, hostile int 0x80 registers).
//!
//! All `Safe` and rerunnable: every test makes its own process and ends it; every wait has a deadline.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hal::entry::segs;
use ktest::{TestResult, kassert, ktest};

const SEC: u64 = 1_000_000_000;

fn have(path: &str) -> bool {
    vfs::root_ns().lookup(path).is_ok()
}

fn diff(got: &str, want: &str) -> Vec<String> {
    let g: Vec<&str> = got.lines().collect();
    let mut d = Vec::new();
    for (i, w) in want.lines().enumerate() {
        match g.get(i) {
            Some(l) if *l == w => {}
            Some(l) => d.push(format!("line {}: got {l:?}, want {w:?}", i + 1)),
            None => d.push(format!("line {}: missing, want {w:?}", i + 1)),
        }
    }
    if g.len() > want.lines().count() {
        d.push(format!("{} extra lines", g.len() - want.lines().count()));
    }
    d
}

ktest!("ab.ab10a.programs", Safe, programs);
fn programs() -> TestResult {
    if !have("/bin/abi-wow64") {
        if have("/bin/abi-ids") {
            return Err(String::from("the image has ABI programs but not /bin/abi-wow64 (did it fail to build?)"));
        }
        return ktest::skip("the user/abi-tests programs are not in this image (no musl sysroot when it was built)");
    }
    let modes: [(&str, &str); 10] = [
        ("tls", include_str!("../../../user/abi-tests/expected/wow64-tls.txt")),
        ("ldt", include_str!("../../../user/abi-tests/expected/wow64-ldt.txt")),
        ("enosys", include_str!("../../../user/abi-tests/expected/wow64-enosys.txt")),
        ("selectors", include_str!("../../../user/abi-tests/expected/wow64-selectors.txt")),
        ("code32", include_str!("../../../user/abi-tests/expected/wow64-code32.txt")),
        ("threads", include_str!("../../../user/abi-tests/expected/wow64-threads.txt")),
        ("fork", include_str!("../../../user/abi-tests/expected/wow64-fork.txt")),
        ("exec", include_str!("../../../user/abi-tests/expected/wow64-exec.txt")),
        ("hostile", include_str!("../../../user/abi-tests/expected/wow64-hostile.txt")),
        ("ldtr", include_str!("../../../user/abi-tests/expected/wow64-ldtr.txt")),
    ];
    for (mode, want) in modes {
        let out = proc::stdio::Capture::new(1 << 16);
        let argv = ["/bin/abi-wow64", mode];
        let env = ["HOME=/", "PATH=/bin", "ABI_TEST=1"];
        let mut o = proc::SpawnOptions::new(&argv, &env, policy::ProgramId(1));
        o.stdin = Some(proc::stdio::Input::new(Vec::new()) as Arc<dyn vfs::File>);
        o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
        o.stderr = Some(out.clone() as Arc<dyn vfs::File>);
        let p = proc::spawn_with("/bin/abi-wow64", &o).map_err(|e| format!("spawn: {e}"))?;
        let code = match p.wait(Some(120 * SEC)) {
            Some(c) => c,
            None => {
                p.kill(9);
                let _ = p.wait(Some(10 * SEC));
                return Err(format!("abi-wow64 {mode} did not end within 120 s:\n{}", String::from_utf8_lossy(&out.contents())));
            }
        };
        let text = String::from_utf8_lossy(&out.contents()).into_owned();
        let d = diff(&text, want);
        kassert!(code == 0 && d.is_empty(), "abi-wow64 {mode} differs from CachyOS (exit {code}):\n{}", d.join("\n"));
        klog::event!(Info, "ktest", "ab10a.program", mode = mode, lines = text.lines().count());
    }
    Ok(())
}

ktest!("ab.ab10a.gdt_layout", Safe, gdt_layout);
fn gdt_layout() -> TestResult {
    use hal::gdt;
    kassert!((gdt::USER32_CS, gdt::USER_DS, gdt::USER_CS) == (0x23, 0x2b, 0x33), "Linux user selectors");
    kassert!(gdt::TSS_SEL == 0x40 && gdt::LDT_SEL == 0x50, "TSS/LDT selectors");
    let t = gdt::this();
    // 32-bit user code: present, ring 3, D=1, L=0; 64-bit: L=1, D=0; user data: present, ring 3, data.
    let (c32, c64, d) = (t.entry(4), t.entry(6), t.entry(5));
    kassert!(c32 >> 45 & 3 == 3 && c32 >> 47 & 1 == 1 && c32 >> 53 & 1 == 0 && c32 >> 54 & 1 == 1, "32-bit code {c32:#x}");
    kassert!(c64 >> 53 & 1 == 1 && c64 >> 54 & 1 == 0, "64-bit code {c64:#x}");
    kassert!(d >> 44 & 1 == 1 && d >> 43 & 1 == 0 && d >> 45 & 3 == 3, "user data {d:#x}");
    // The running (kernel) thread has no LDT and the null LDTR.
    kassert!(gdt::ldtr() == 0, "LDTR {:#x} not null", gdt::ldtr());
    kassert!(t.ldt_base() == 0, "LDT base not 0");
    Ok(())
}

ktest!("ab.ab10a.descriptors", Safe, descriptors);
fn descriptors() -> TestResult {
    use crate::ldt::UserDesc;
    // Wine's 4 GiB writable 32-bit data segment: fill_ldt's bytes, loadable into a data register at RPL 3.
    let u = UserDesc { entry_number: 0, base_addr: 0x1234_5678, limit: 0xfffff, flags: 1 | 1 << 4 | 1 << 6 };
    let desc = u.descriptor();
    kassert!(desc == 0x12DF_F334_5678_FFFF, "fill_ldt descriptor {desc:#x}");
    kassert!(segs::desc_loadable(desc, 3), "a program may load it");
    kassert!(segs::desc_base(desc) == 0x1234_5678, "base");
    let back = UserDesc::of_descriptor(12, desc);
    kassert!(back == UserDesc { entry_number: 12, ..u }, "read back as written");
    // A kernel data descriptor is not loadable at RPL 3; the empty one never is.
    kassert!(!segs::desc_loadable(hal::gdt::this().entry(2), 3), "kernel data refused at RPL 3");
    kassert!(!segs::desc_loadable(0, 3), "empty refused");
    Ok(())
}
