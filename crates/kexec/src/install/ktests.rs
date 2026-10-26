//! In-kernel tests of RB-6.
//!
//! Safe (nothing written; rerunnable; the Mini-PC runs them over MCP):
//! - `rb.install.checks`: the bundle checks on synthetic files (kernel with and without the signature, cut
//!   short, an initramfs without its trailer, guarded CONFIG lines), the plan of a typical stick.
//! - `rb.install.dry_run`: on the real boot stick, a dry run of an install with a synthetic bundle (listing,
//!   PIN, CONFIG, free space, plan, next-boot prediction): the stick's listing is the same before and after;
//!   the confirmation status agrees with the GENERATION record.
//!
//! QemuOnly (writes the stick image of this run, and puts everything back):
//! - `rb.install.roundtrip`: the MCP tool `install_kernel` fetches a synthetic bundle from an HTTP server inside
//!   the kernel and installs it (generation N, NEW, both files read back equal), a second install replaces it as
//!   N+1, a CONFIG.TXT with `usb_msd_write=on` is refused with the stick unchanged, a harmless CONFIG.TXT is written,
//!   the confirmation of N+1 writes OK and clears NEW; then the generation, CONFIG.TXT and the markers are put
//!   back as they were.

use super::plan::{self, Gen};
use super::{FatStick, LOCK, Opts, Stick, check, confirm, confirm_on, install_on};
use crate::ktests::{Server, ok_body};
use crate::testkit::{Seg, elf};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use bootinfo::layout::KERNEL_BASE;
use fs_fat::sysapi::Marker;
use ktest::{TestResult, kassert, ktest};

const SEC: u64 = 1_000_000_000;

pub(super) fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

/// A small kernel-like ELF that passes every install check (tag makes two differ).
pub(super) fn kernel(tag: u8) -> Vec<u8> {
    let text: Vec<u8> = (0..5000u32).map(|i| (i as u8).wrapping_mul(13) ^ tag).collect();
    let mut rodata = check::SIGNATURE.to_vec();
    rodata.extend_from_slice(b": entry (rb.install ktest)");
    rodata.push(tag);
    elf(
        &[
            Seg { vaddr: KERNEL_BASE, flags: 5, data: &text, memsz: text.len() as u64 },
            Seg { vaddr: KERNEL_BASE + 0x2000, flags: 4, data: &rodata, memsz: rodata.len() as u64 },
            Seg { vaddr: KERNEL_BASE + 0x3000, flags: 6, data: &[tag; 32], memsz: 0x1000 },
        ],
        KERNEL_BASE + 0x20,
    )
}

fn newc_entry(out: &mut Vec<u8>, name: &str, body: &[u8], mode: u32) {
    let h = format!(
        "070701{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
        1,
        mode,
        0,
        0,
        1,
        0,
        body.len(),
        0,
        0,
        0,
        0,
        name.len() + 1,
        0
    );
    out.extend_from_slice(h.as_bytes());
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out.extend_from_slice(body);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

pub(super) fn initrd(tag: u8) -> Vec<u8> {
    let mut v = Vec::new();
    newc_entry(&mut v, "rb6/ktest", &[tag; 1000], 0o100644);
    newc_entry(&mut v, "TRAILER!!!", b"", 0);
    v
}

fn rb_install_checks() -> TestResult {
    check::kernel(&kernel(1)).map_err(e)?;
    let k = kernel(1);
    let end = crate::elf::parse_head(&k).map_err(e)?.ok_or("no layout")?.file_end() as usize;
    kassert!(check::kernel(&k[..end - 1]).is_err(), "a cut-short kernel passed");
    let unsigned = elf(&[Seg { vaddr: KERNEL_BASE, flags: 5, data: &[0x90; 64], memsz: 64 }], KERNEL_BASE);
    kassert!(check::kernel(&unsigned).is_err_and(|t| t.contains("signature")));
    let i = initrd(1);
    check::initrd(&i).map_err(e)?;
    kassert!(check::initrd(&i[..i.len() - 4]).is_err(), "an initramfs without its trailer passed");
    kassert!(check::config(Some(b"token=a\n"), b"token=a\nusb_msd_write=on\n").is_err());
    kassert!(check::config(Some(b"token=a\n"), b"token=a\nnvme_write=on\n").is_err());
    check::config(Some(b"token=a\n"), b"token=a\nwlan=on\n").map_err(e)?;
    check::config(Some(b"token=a\nnvme_write=off\n"), b"token=a\n").map_err(e)?;
    let g = |n, ok, new| Gen { n, ok, new, kernel: true, initrd: true, ..Gen::default() };
    let p = plan::install(&[g(1, true, false), g(2, false, true)], None, 5, false, false).map_err(|x| x.describe())?;
    kassert!((p.number, p.replaced, p.fallback) == (3, Some(2), Some(1)), "{p:?}");
    Ok(())
}
ktest!("rb.install.checks", Safe, rb_install_checks);

pub(super) fn mounted() -> Result<alloc::sync::Arc<fs_fat::Volume>, TestResult> {
    match fs_fat::stick::wait_mounted(40_000) {
        Some(v) => Ok(v),
        None => Err(match fs_fat::stick::last_reason() {
            Some(r) => Err(format!("the boot stick was not mounted: {r}")),
            None => ktest::skip("no boot stick (no USB disk recognised as the stick)"),
        }),
    }
}

pub(super) fn snapshot() -> Result<(Vec<Gen>, Option<u32>, Option<Vec<u8>>), String> {
    fs_fat::stick::with_sys(|s| {
        let st = FatStick(s);
        Ok::<_, i64>((st.generations()?, st.pin()?, st.config()?))
    })
    .map_err(e)?
    .map_err(e)
}

fn rb_install_dry_run() -> TestResult {
    if let Err(r) = mounted() {
        return r;
    }
    let before = snapshot()?;
    let _l = LOCK.lock_timeout(60 * SEC).ok_or("the install lock is held")?;
    let r = fs_fat::stick::with_sys(|s| install_on(&FatStick(s), &kernel(3), &initrd(3), None, &Opts { dry_run: true, allow_no_fallback: true, ..Opts::default() }))
        .map_err(e)?;
    let after = snapshot()?;
    kassert!(before == after, "a dry run changed the stick: {before:?} -> {after:?}");
    let r = r.map_err(|x| String::from(x.text()))?;
    let top = before.0.iter().map(|g| g.n).max().unwrap_or(0);
    kassert!(r.plan.number == top + 1, "number {} after {top}", r.plan.number);
    let next = plan::next_boot(&before.0, before.1);
    klog::event!(Info, "kexec", "ktest.install_dry_run", generations = before.0.iter().map(|g| format!("{} ({})", g.n, g.markers())).collect::<Vec<_>>().join(", "), number = r.plan.number,
        replaced = r.plan.replaced.unwrap_or(0), fallback = r.plan.fallback.unwrap_or(0), next_boot = format!("{next:?}"));
    // The confirmation agrees with what bitsboot handed this kernel.
    if let Some((bi, _)) = crate::boot::boot_info() {
        let counter = bi.reload().map(|x| x.counter).unwrap_or(0);
        let want = confirm::needs_confirmation(counter, bi.generation());
        let st = confirm::status();
        match want {
            Ok(n) => kassert!(
                !matches!(st, confirm::Status::NotNeeded(_) | confirm::Status::Unknown),
                "generation {n} needs its confirmation but the status is {st:?}"
            ),
            Err(_) => kassert!(matches!(st, confirm::Status::NotNeeded(_)), "status {st:?}"),
        }
    }
    Ok(())
}
ktest!("rb.install.dry_run", Safe, rb_install_dry_run);

fn json_str(b: &[u8]) -> String {
    let mut s = String::from("\"");
    for &c in b {
        match c {
            b'"' => s.push_str("\\\""),
            b'\\' => s.push_str("\\\\"),
            b'\n' => s.push_str("\\n"),
            b'\r' => s.push_str("\\r"),
            0x20..=0x7e => s.push(c as char),
            _ => s.push_str(&format!("\\u{:04x}", c)),
        }
    }
    s.push('"');
    s
}

fn call(args: &str) -> Result<String, String> {
    let a = mcp::Json::parse(args).map_err(e)?;
    let ctx = mcp::ToolContext::detached(time::now_ns() + 60 * SEC);
    match super::tool::install_kernel(&ctx, &a) {
        Ok(out) => Ok(out
            .iter()
            .filter_map(|o| match o {
                mcp::ToolOutput::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")),
        Err(x) => Err(format!("{x:?}")),
    }
}

/// What the roundtrip changed, put back in its Drop (also when the test fails half way).
struct Restore {
    made: Vec<u32>,
    marked_ok: Option<u32>,
    config: Option<Vec<u8>>,
}

impl Drop for Restore {
    fn drop(&mut self) {
        let _l = LOCK.lock_timeout(60 * SEC);
        let _ = fs_fat::stick::with_sys(|s| {
            let st = FatStick(s);
            for &n in &self.made {
                let _ = st.remove_generation(n);
            }
            if let Some(n) = self.marked_ok {
                let _ = st.set_marker(n, Marker::Ok, false);
            }
            if let Some(c) = &self.config {
                let _ = st.write_config(c);
            }
        });
    }
}

fn rb_install_roundtrip() -> TestResult {
    let vol = match mounted() {
        Ok(v) => v,
        Err(r) => return r,
    };
    kassert!(vol.writable(), "the stick is read-only: {}", fs_fat::stick::last_reason().unwrap_or("?"));
    let (gens, _pin, config) = snapshot()?;
    let base = gens.iter().map(|g| g.n).max().ok_or("the stick holds no generation")?;
    let mut restore = Restore { made: Vec::new(), marked_ok: None, config: config.clone() };
    // A fallback must exist for an install without allow_no_fallback: the image's generation is unmarked.
    if !gens.iter().any(|g| g.ok) {
        fs_fat::stick::with_sys(|s| s.set_marker(base, Marker::Ok, true)).map_err(e)?.map_err(e)?;
        restore.marked_ok = Some(base);
    }
    let (k1, i1, k2, i2) = (kernel(1), initrd(1), kernel(2), initrd(2));
    let srv = Server::start(
        23061,
        vec![("/a/KERNEL", ok_body(&k1)), ("/a/INITRD", ok_body(&i1)), ("/b/KERNEL", ok_body(&k2)), ("/b/INITRD", ok_body(&i2))],
    )?;
    // 1. Install a: generation base+1, NEW, files equal.
    let n = base + 1;
    restore.made.push(n);
    let t = call(&format!("{{\"url\":\"{}\"}}", srv.url("/a/")))?;
    kassert!(t.contains(&format!("installed generation {n} (NEW)")), "{t}");
    kassert!(t.contains("read back from the stick and equal"), "{t}");
    let files = |g: u32| -> Result<(Vec<u8>, Vec<u8>, Gen), String> {
        fs_fat::stick::with_sys(|s| -> Result<_, i64> {
            let st = FatStick(s);
            let gg = st.generations()?.into_iter().find(|x| x.n == g).ok_or(uapi::errno::ENOENT)?;
            Ok((s.read_file(g, "KERNEL", 1 << 20)?, s.read_file(g, "INITRD", 1 << 20)?, gg))
        })
        .map_err(e)?
        .map_err(e)
    };
    let (k, i, gg) = files(n)?;
    kassert!(k == k1 && i == i1, "generation {n}: files differ from the bundle");
    kassert!(gg.new && !gg.ok && !gg.tried, "generation {n}: markers {}", gg.markers());
    // 2. A guarded CONFIG line is refused and nothing changes.
    let before = snapshot()?;
    let mut bad = config.clone().unwrap_or_default();
    bad.extend_from_slice(b"\nusb_msd_write=on\n");
    let r = call(&format!("{{\"url\":\"{}\",\"config\":{}}}", srv.url("/b/"), json_str(&bad)));
    kassert!(r.as_ref().is_err_and(|t| t.contains("usb_msd_write") && t.contains("nothing on the stick changed")), "{r:?}");
    kassert!(snapshot()? == before, "a refused install changed the stick");
    // 3. Install b with a harmless CONFIG change: the unconfirmed n is replaced by n+1 (never the same number).
    let mut good = config.clone().unwrap_or_default();
    good.extend_from_slice(b"\n# rb.install.roundtrip was here\n");
    restore.made.push(n + 1);
    let t = call(&format!("{{\"url\":\"{}\",\"config\":{}}}", srv.url("/b/"), json_str(&good)))?;
    kassert!(t.contains(&format!("installed generation {} (NEW)", n + 1)) && t.contains(&format!("unconfirmed generation {n} was removed")), "{t}");
    kassert!(t.contains("CONFIG.TXT written"), "{t}");
    let (gens2, _, cfg2) = snapshot()?;
    kassert!(!gens2.iter().any(|g| g.n == n), "generation {n} is still there");
    kassert!(cfg2.as_deref() == Some(&good[..]), "CONFIG.TXT not as written");
    let (k, i, _) = files(n + 1)?;
    kassert!(k == k2 && i == i2, "generation {}: files differ from the bundle", n + 1);
    // 4. The confirmation of n+1 (what its kernel does after 2 minutes of MCP).
    {
        let _l = LOCK.lock_timeout(60 * SEC).ok_or("the install lock is held")?;
        let c = fs_fat::stick::with_sys(|s| confirm_on(&FatStick(s), n + 1)).map_err(e)??;
        kassert!(matches!(c, super::Confirmed::Now { .. }), "{c:?}");
        let c = fs_fat::stick::with_sys(|s| confirm_on(&FatStick(s), n)).map_err(e)??;
        kassert!(c == super::Confirmed::Missing, "{c:?}");
    }
    let (_, _, gg) = files(n + 1)?;
    kassert!(gg.ok && !gg.new && !gg.tried, "generation {}: markers {}", n + 1, gg.markers());
    drop(srv);
    drop(restore);
    let after = snapshot()?;
    kassert!(after.0 == gens && after.2 == config, "the stick was not put back: {:?} -> {:?}", gens, after.0);
    Ok(())
}
ktest!("rb.install.roundtrip", QemuOnly, rb_install_roundtrip);
