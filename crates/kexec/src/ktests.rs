//! Reload's ktests: header checks and refusals (bad ELF, too big, wrong URL scheme, not LAN, HTTP errors) through
//! an HTTP server on 127.0.0.1 inside the kernel, a full staging of a small synthetic kernel (dry run, its page
//! tables walked with the MMU's rules), and the memory squeezes the Reload scenario uses (QemuOnly, CONFIG keys).

use crate::elf::{ElfError, ElfStream, ImageSink, Layout, parse_head};
use crate::stage;
use crate::url::{self, UrlError};
use crate::{boot, testkit};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use bootinfo::layout::{DIRECT_MAP_BASE, KERNEL_BASE};
use bootinfo::{BootInfo, memtype, module_kind};
use core::sync::atomic::{AtomicBool, Ordering};
use ktest::{TestResult, kassert, ktest};
use net::{Ipv4, SockAddr};

const SEC: u64 = 1_000_000_000;

struct Collect(Vec<u8>, u64);
impl ImageSink for Collect {
    fn layout(&mut self, l: &Layout) -> Result<(), String> {
        self.0 = vec![0; l.span as usize];
        self.1 = l.lo;
        Ok(())
    }
    fn write(&mut self, off: u64, d: &[u8]) -> Result<(), String> {
        self.0[off as usize..off as usize + d.len()].copy_from_slice(d);
        Ok(())
    }
}

fn load(file: &[u8], piece: usize) -> Result<(Layout, Vec<u8>), ElfError> {
    let mut s = ElfStream::new();
    let mut c = Collect(Vec::new(), 0);
    for p in file.chunks(piece.max(1)) {
        s.feed(p, &mut c)?;
    }
    Ok((s.finish()?, c.0))
}

fn elf_checks() -> TestResult {
    let good = testkit::small_kernel();
    let (l, img) = load(&good, good.len()).map_err(|e| format!("good kernel refused: {}", e.describe()))?;
    kassert!(l.entry == KERNEL_BASE + 0x10 && l.lo == KERNEL_BASE && l.span == 0x6000, "layout {:?}", l);
    for piece in [1, 7, 4096] {
        let (_, again) = load(&good, piece).map_err(|e| e.describe())?;
        kassert!(again == img, "fed in {piece}-byte pieces: other image");
    }
    let mut bad = good.clone();
    bad[0] = b'X';
    kassert!(load(&bad, 64).unwrap_err() == ElfError::NotElf);
    let mut bad = good.clone();
    bad[18] = 0x28;
    kassert!(load(&bad, 64).unwrap_err() == ElfError::NotX86_64);
    let mut bad = good.clone();
    bad[16] = 3;
    kassert!(load(&bad, 64).unwrap_err() == ElfError::NotExecutable);
    let mut bad = good.clone();
    bad[24..32].copy_from_slice(&(KERNEL_BASE + 0x2010).to_le_bytes());
    kassert!(matches!(load(&bad, 64).unwrap_err(), ElfError::EntryNotExecutable { .. }), "entry in rodata");
    let low = testkit::elf(&[testkit::Seg { vaddr: 0x40_0000, flags: 5, data: &[0x90; 16], memsz: 16 }], 0x40_0000);
    kassert!(matches!(load(&low, 64).unwrap_err(), ElfError::BelowKernelBase { .. }));
    let huge = testkit::elf(&[testkit::Seg { vaddr: KERNEL_BASE, flags: 5, data: &[0x90; 16], memsz: 2 << 30 }], KERNEL_BASE);
    kassert!(matches!(load(&huge, 64).unwrap_err(), ElfError::SpanTooLarge { .. } | ElfError::Overflow));
    kassert!(matches!(load(&good[..0x4000 + 50], 4096).unwrap_err(), ElfError::Truncated { .. }), "truncated");
    let mut far = good.clone();
    far[32..40].copy_from_slice(&(70_000u64).to_le_bytes());
    kassert!(matches!(parse_head(&far).unwrap_err(), ElfError::HeadersTooFar { .. }));
    Ok(())
}
ktest!("rb.reload.elf_checks", Safe, elf_checks);

fn url_refusals() -> TestResult {
    for (u, want) in [
        ("ftp://192.168.100.20/KERNEL", "scheme"),
        ("https://192.168.100.20/KERNEL", "https"),
        ("http://example.com/KERNEL", "address"),
        ("http://8.8.8.8/KERNEL", "lan"),
        ("192.168.100.20/KERNEL", "bad"),
    ] {
        let e = url::bundle(u, None).unwrap_err();
        let ok = match want {
            "scheme" => matches!(e, UrlError::Scheme(_)),
            "https" => e == UrlError::Https,
            "address" => e == UrlError::NeedsAddress,
            "lan" => matches!(e, UrlError::NotLan(_)),
            _ => matches!(e, UrlError::Bad(_)),
        };
        kassert!(ok, "{u}: {e:?}");
    }
    let args = mcp::Json::parse(r#"{"url":"ftp://10.0.2.2/x"}"#).map_err(|_| String::from("json"))?;
    kassert!(matches!(crate::tool::bundle_from(&args), Err(mcp::ToolError::Refused(_))), "tool refuses ftp");
    let b = url::bundle("http://10.0.2.2:23010/b/", None).map_err(|e| e.describe())?;
    kassert!(b.kernel == "http://10.0.2.2:23010/b/KERNEL" && b.initrd.as_deref() == Some("http://10.0.2.2:23010/b/INITRD"));
    Ok(())
}
ktest!("rb.reload.url_refusals", Safe, url_refusals);

/// A one-shot HTTP server on 127.0.0.1: answers each request with the canned response for its path.
pub(crate) struct Server {
    port: u16,
    stop: Arc<AtomicBool>,
}

impl Server {
    pub(crate) fn start(port: u16, routes: Vec<(&'static str, Vec<u8>)>) -> Result<Server, String> {
        let sock = net::stack().ok_or("no network stack")?;
        let l = sock.tcp_listen(SockAddr { ip: Ipv4::LOCALHOST, port }, 4).map_err(|e| format!("listen: {e:?}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        sched::spawn("rb-ktest-http", sched::Class::Normal, move || {
            while !st.load(Ordering::Acquire) {
                let Ok(c) = l.accept(Some(SEC / 5)) else { continue };
                let mut req = Vec::new();
                let mut buf = [0u8; 512];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") && req.len() < 8192 {
                    match c.read(&mut buf, Some(5 * SEC)) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let line = String::from_utf8_lossy(&req);
                let path = line.split(' ').nth(1).unwrap_or("/");
                let resp = routes.iter().find(|r| r.0 == path).map(|r| r.1.as_slice()).unwrap_or(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                let _ = c.write_all(resp, Some(5 * SEC));
                c.close();
            }
        })
        .map_err(|e| format!("spawn: {e:?}"))?;
        Ok(Server { port, stop })
    }
    pub(crate) fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

pub(crate) fn ok_body(b: &[u8]) -> Vec<u8> {
    let mut v = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", b.len()).into_bytes();
    v.extend_from_slice(b);
    v
}

fn bundle(kernel: String, initrd: Option<String>) -> url::Bundle {
    url::Bundle { kernel, initrd }
}

fn deadline() -> u64 {
    time::now_ns() + 30 * SEC
}

fn need_boot_info() -> Result<(BootInfo<'static>, u64), String> {
    boot::boot_info().ok_or_else(|| String::from("the kernel does not call kexec::set_boot_info (desk change request of RB-5a)"))
}

fn http_refusals() -> TestResult {
    if need_boot_info().is_err() {
        return ktest::skip("the kernel does not call kexec::set_boot_info (desk change request of RB-5a)");
    }
    let initrd = vec![0x42u8; 3000];
    let srv = Server::start(
        23051,
        vec![
            ("/junk", ok_body(&[0x55; 2048])),
            ("/huge", b"HTTP/1.1 200 OK\r\nContent-Length: 4398046511104\r\n\r\n".to_vec()),
            ("/short", {
                let k = testkit::small_kernel();
                let mut v = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
                v.extend_from_slice(&k[..0x4000 + 50]);
                v
            }),
            ("/kernel", ok_body(&testkit::small_kernel())),
            ("/initrd", ok_body(&initrd)),
        ],
    )?;
    let refused = |b: url::Bundle, cap: u64, want: &str| -> TestResult {
        match stage::stage_with(&b, deadline(), cap) {
            Ok(_) => Err(format!("{}: staged, want a refusal ({want})", b.kernel)),
            Err(e) => {
                kassert!(e.text().contains(want), "{}: {:?} (want '{want}')", b.kernel, e);
                Ok(())
            }
        }
    };
    refused(bundle(srv.url("/junk"), None), u64::MAX, "not an ELF")?;
    refused(bundle(srv.url("/missing"), None), u64::MAX, "HTTP 404")?;
    refused(bundle(srv.url("/huge"), None), u64::MAX, "too big")?;
    refused(bundle(srv.url("/short"), None), u64::MAX, "ended")?;
    refused(bundle(srv.url("/kernel"), Some(srv.url("/huge"))), u64::MAX, "too big")?;
    refused(bundle(srv.url("/kernel"), Some(srv.url("/initrd"))), 16 * 1024, "does not fit")?;
    refused(bundle(srv.url("/kernel"), Some(srv.url("/initrd"))), 40 * 1024, "does not fit")?;
    // Nothing of the refused attempts is held: the next one succeeds and gives everything back on drop.
    let before = mm_core::frames().map(|f| f.stats().used_by_kind[mm_core::Kind::Other as usize]).unwrap_or(0);
    let st = stage::stage(&bundle(srv.url("/kernel"), Some(srv.url("/initrd"))), deadline()).map_err(|e| format!("{e:?}"))?;
    drop(st);
    let after = mm_core::frames().map(|f| f.stats().used_by_kind[mm_core::Kind::Other as usize]).unwrap_or(0);
    kassert!(after == before, "frames kept after drop: {before} -> {after}");
    Ok(())
}
ktest!("rb.reload.http_refusals", Safe, http_refusals);

/// Stages the small synthetic kernel (dry run) and checks everything the new kernel would get.
fn stage_synthetic() -> TestResult {
    let (bi, bi_phys) = match need_boot_info() {
        Ok(b) => b,
        Err(e) => return ktest::skip(&e),
    };
    let initrd: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    let kernel = testkit::small_kernel();
    let srv = Server::start(23052, vec![("/k", ok_body(&kernel)), ("/i", ok_body(&initrd))])?;
    let st = stage::stage(&bundle(srv.url("/k"), Some(srv.url("/i"))), deadline()).map_err(|e| format!("{e:?}"))?;
    let s = st.summary.clone();
    let counter = bi.reload().map(|r| r.counter).unwrap_or(0);
    kassert!(s.counter == counter + 1, "counter {} after {}", s.counter, counter);
    kassert!(s.entry == KERNEL_BASE + 0x10 && s.span == 0x6000);
    let mut fnv = crate::Fnv::new();
    fnv.update(&initrd);
    kassert!(s.initrd_fnv == fnv.finish(), "initrd hash");
    let (ip, isize) = s.initrd.ok_or("no initrd")?;
    kassert!(isize == initrd.len() as u64 && boot::fnv_phys(ip, isize) == Some(fnv.finish()), "initrd bytes in their block");

    // The BootInfo the new kernel gets.
    let bytes = st.bootinfo_bytes();
    let nb = BootInfo::parse(bytes).map_err(|e| format!("new BootInfo: {e:?}"))?;
    kassert!(nb.reload().map(|r| r.counter) == Some(counter + 1));
    kassert!(nb.config() == bi.config(), "CONFIG handed on unchanged");
    kassert!(nb.edid() == bi.edid(), "EDID handed on unchanged");
    kassert!(nb.crash_page() == bi.crash_page() && nb.reload_stub() == bi.reload_stub() && nb.trampoline() == bi.trampoline(), "KEEP records");
    kassert!(nb.tsc().map(|t| t.calibrated_hz) == Some(time::tsc_hz()), "TSC rate");
    let m: Vec<_> = nb.modules().filter(|m| m.kind == module_kind::INITRAMFS).collect();
    kassert!(m.len() == 1 && m[0].phys == ip && m[0].size == isize, "initramfs module {:?}", m);
    let k = nb.kernel_image().ok_or("no KERNEL_IMAGE")?;
    kassert!(k.phys_base == s.image_phys && k.entry == s.entry && k.virt_base == KERNEL_BASE);
    let map = nb.memory_map().ok_or("no memory map")?;
    let ty_at = |p: u64| map.iter().find(|d| d.phys <= p && p < d.end()).map(|d| d.ty);
    kassert!(ty_at(s.image_phys) == Some(memtype::BITS_KERNEL), "image is KERNEL");
    kassert!(ty_at(ip) == Some(memtype::BITS_MODULE), "initrd is MODULE");
    let pt = nb.page_tables().ok_or("no PAGE_TABLES")?;
    kassert!(ty_at(pt.pml4_phys) == Some(memtype::BITS_PAGETABLE), "tables are PAGETABLE");
    kassert!(ty_at(bi_phys) == Some(memtype::BITS_KEEP), "BootInfo stays KEEP");
    if let Some(old) = bi.kernel_image() {
        kassert!(ty_at(old.phys_base) == Some(memtype::CONVENTIONAL), "the running kernel's image is free for the new one");
    }
    for old in bi.modules().filter(|m| m.kind == module_kind::INITRAMFS) {
        if !boot::initrd_released() {
            kassert!(ty_at(old.phys) == Some(memtype::CONVENTIONAL), "the replaced initramfs is free");
            continue;
        }
        // INT-A1: the running kernel gave the old initramfs back to its allocator, so the frames this stage got
        // (image, stack, tables, the new initrd) may lie inside it. Free or one of those, never the old module.
        const PAGE: u64 = 4096;
        let (os, oe) = (old.phys & !(PAGE - 1), (old.phys + old.size).next_multiple_of(PAGE));
        let (ns, ne) = (ip & !(PAGE - 1), (ip + isize).next_multiple_of(PAGE));
        for d in map.iter().filter(|d| d.phys < oe && d.end() > os) {
            let ok = match d.ty {
                memtype::CONVENTIONAL | memtype::BITS_KERNEL | memtype::BITS_PAGETABLE => true,
                memtype::BITS_MODULE => d.phys.max(os) >= ns && d.end().min(oe) <= ne,
                _ => false,
            };
            kassert!(ok, "the replaced initramfs [{os:#x}, {oe:#x}) holds {:#x}+{} pages of type {:#x}", d.phys, d.pages, d.ty);
        }
    }

    // The new tables, walked by the MMU's rules (hal's translate over the new root).
    use hal::paging::{PageFlags, translate};
    let root = pt.pml4_phys;
    let walk = |v: u64| translate(root, v);
    let (p, f) = walk(KERNEL_BASE + 0x10).ok_or("entry not mapped")?;
    kassert!(p == s.image_phys + 0x10 && !f.contains(PageFlags::NX) && !f.contains(PageFlags::WRITABLE), "text R X");
    let mut word = [0u8; 4];
    hal::kexec_stub::read_phys(p - 0x10, &mut word);
    kassert!(word == [1, 8, 15, 22], "text bytes in place: {:?}", word);
    let (_, f) = walk(KERNEL_BASE + 0x2000).ok_or("rodata not mapped")?;
    kassert!(f.contains(PageFlags::NX) && f.contains(PageFlags::WRITABLE), "rodata page shared with data: W from data, NX");
    let (p, f) = walk(KERNEL_BASE + 0x4000).ok_or("bss not mapped")?;
    kassert!(p == s.image_phys + 0x4000 && f.contains(PageFlags::WRITABLE) && f.contains(PageFlags::NX), "bss RW NX");
    let (p, f) = walk(DIRECT_MAP_BASE + bi_phys).ok_or("BootInfo not in the direct map")?;
    kassert!(p == bi_phys && f.contains(PageFlags::NX));
    let stub = bi.reload_stub().ok_or("no stub")?.phys;
    let (p, f) = walk(stub).ok_or("stub not identity-mapped")?;
    kassert!(p == stub && !f.contains(PageFlags::NX), "stub identity, executable");
    // NX on any level above the leaf counts too (translate reports the leaf's only).
    kassert!(hal::kexec_stub::executable(root, stub) == Some((stub, true)), "stub executable on every level of the new tables");
    kassert!(hal::kexec_stub::executable(root, DIRECT_MAP_BASE + bi_phys).is_some_and(|(_, x)| !x), "direct map no-execute");
    if let Some(fb) = bi.framebuffer().filter(|f| f.phys != 0) {
        let (p, f) = walk(bootinfo::layout::VMAP_BASE).ok_or("framebuffer not mapped")?;
        kassert!(p == fb.phys & !0xFFF && f.contains(PageFlags::PWT), "framebuffer at VMAP_BASE, PAT 1");
    }
    kassert!(walk(0x1000).is_none() && walk(KERNEL_BASE + 0x6000).is_none(), "nothing else mapped");
    // The running kernel's own code must stay reachable for the stub's jump: same check the tool makes (also from
    // a thread on a vmap stack, `rb.reload.stub_ready_on_thread`).
    kassert!(crate::stage::stub_ready(stub).is_ok(), "stub page usable for the jump: {:?}", crate::stage::stub_ready(stub));
    let code = hal::kexec_stub::stub_code();
    kassert!(code.len() < 256 && code.len() > 24, "stub {} bytes", code.len());
    kassert!(code[code.len() - 16..] == [0xFF, 0xFF, 0, 0, 0, 0x9B, 0xAF, 0, 0xFF, 0xFF, 0, 0, 0, 0x93, 0xCF, 0], "stub ends with its GDT (accessed bits preset)");
    // The jump's checks pass before anything is stopped or quiesced, with APs online too (RB-5b stops them
    // after this check); the jump itself still needs every AP sent INIT (cpu_ktests).
    kassert!(st.check().is_ok(), "the jump's checks pass before anything is quiesced: {:?}", st.check());
    drop(st);
    Ok(())
}
ktest!("rb.reload.stage_synthetic", Safe, stage_synthetic);

/// RB-10 (Safe, rerunnable, reads the stick only): the staged BootInfo refreshed as at the jump keeps every
/// record but CONFIG, passes the jump's checks, and a CONFIG that does not fit the KEEP block is refused.
fn config_refresh() -> TestResult {
    let (bi, _) = match need_boot_info() {
        Ok(b) => b,
        Err(e) => return ktest::skip(&e),
    };
    let kernel = testkit::small_kernel();
    let initrd: Vec<u8> = (0..4096u32).map(|i| (i % 13) as u8).collect();
    let srv = Server::start(23053, vec![("/k", ok_body(&kernel)), ("/i", ok_body(&initrd))])?;
    let mut st = stage::stage(&bundle(srv.url("/k"), Some(srv.url("/i"))), deadline()).map_err(|e| format!("{e:?}"))?;
    let cap = st.summary.bootinfo_capacity;
    // What the jump would do with the stick as it is (no stick: the boot copy, logged).
    let p = crate::refresh::plan(cap, true, None);
    let r = crate::refresh::rebuild(st.bootinfo_bytes(), cap, &p);
    let boot_copy = bi.config().map(<[u8]>::to_vec).unwrap_or_default();
    let want = if r.config { p.config.clone().unwrap_or_default() } else { boot_copy.clone() };
    crate::refresh::apply(&mut st);
    let nb = BootInfo::parse(st.bootinfo_bytes()).map_err(|e| format!("refreshed BootInfo: {e:?}"))?;
    kassert!(nb.config().unwrap_or(b"") == want.as_slice(), "CONFIG as planned ({})", crate::refresh::describe(&p, &r, cap));
    kassert!(st.summary.bootinfo_bytes == st.bootinfo_bytes().len());
    kassert!(st.check().is_ok(), "the jump's checks pass after the refresh: {:?}", st.check());
    // A key added: only CONFIG changes.
    let mut text = want.clone();
    text.extend_from_slice(b"rb_ktest_refresh=1\n");
    let before = st.bootinfo_bytes().to_vec();
    let out = crate::handover::refresh(&before, cap, Some(&text), None).map_err(|e| format!("{e:?}"))?;
    let (a, b) = (BootInfo::parse(&before).map_err(|e| format!("{e:?}"))?, BootInfo::parse(&out).map_err(|e| format!("{e:?}"))?);
    kassert!(b.config() == Some(text.as_slice()), "new CONFIG in");
    kassert!(a.records().count() == b.records().count(), "same records");
    for (x, y) in a.records().zip(b.records()) {
        kassert!(x.tag == y.tag && (x.tag == bootinfo::tag::CONFIG || x.payload == y.payload), "record {} kept", x.tag);
    }
    let big = alloc::vec![b'#'; cap];
    kassert!(crate::handover::refresh(&before, cap, Some(&big), None).is_err(), "a CONFIG beyond the KEEP block is refused");
    refresh_fallbacks(&mut st, &before, big)?;
    drop(st);
    Ok(())
}

/// The jump's swap when the plan cannot be carried out: the staged bytes stay, logged; a CONFIG too large for
/// the KEEP block does not take the confirmed generation with it.
fn refresh_fallbacks(st: &mut stage::Staged, before: &[u8], big: Vec<u8>) -> TestResult {
    use crate::refresh::{Applied, Outcome, Plan, apply_plan};
    let ok = |s: &stage::Staged| s.check().map_err(|e| format!("{e:?}"));
    let plan = |config: Option<Vec<u8>>, generation| Plan { config, outcome: Outcome::Fresh, generation, switched: false, boot_bytes: 0 };
    let kept_since = |seq| klog::read_since(seq, 10_000).iter().filter(|r| r.subsystem == "kexec" && r.name == "reload.config_kept").count();
    let g = bootinfo::Generation { number: 4242, reason: bootinfo::gen_reason::NEWEST_OK, skipped: 0, flags: 0 };
    // Too large alone: nothing changes.
    let seq = klog::next_seq();
    kassert!(apply_plan(st, &plan(Some(big.clone()), None), &ok) == Applied::Kept);
    kassert!(st.bootinfo_bytes() == before, "staged bytes unchanged");
    kassert!(kept_since(seq) == 1, "reload.config_kept logged");
    // Too large with a generation: the generation alone goes in.
    let seq = klog::next_seq();
    kassert!(apply_plan(st, &plan(Some(big), Some(g)), &ok) == Applied::Swapped { config: false, generation: true });
    let (a, b) = (BootInfo::parse(before).map_err(|e| format!("{e:?}"))?, BootInfo::parse(st.bootinfo_bytes()).map_err(|e| format!("{e:?}"))?);
    kassert!(b.config() == a.config() && b.generation() == Some(g), "CONFIG kept, generation in");
    kassert!(st.summary.bootinfo_bytes == st.bootinfo_bytes().len() && st.check().is_ok());
    kassert!(kept_since(seq) == 1, "the dropped CONFIG logged");
    st.replace_bootinfo(before.to_vec());
    // The jump's checks fail on the rebuilt bytes: the staged ones come back.
    let seq = klog::next_seq();
    let failing = |_: &stage::Staged| Err(String::from("injected by the ktest"));
    kassert!(apply_plan(st, &plan(Some(b"rb_ktest_refresh=1\n".to_vec()), Some(g)), &failing) == Applied::Kept);
    kassert!(st.bootinfo_bytes() == before && st.summary.bootinfo_bytes == before.len(), "rolled back");
    kassert!(kept_since(seq) == 1, "the failed check logged");
    kassert!(apply_plan(st, &plan(None, None), &ok) == Applied::Nothing && st.bootinfo_bytes() == before);
    Ok(())
}
ktest!("rb.reload.config_refresh", Safe, config_refresh);

/// The jump's page-table root works for a thread on a vmap stack too (the MCP server's threads; the boot thread
/// runs on the kernel image's stack, which every root maps): found when bitsboot's root was used after MM-2.
fn stub_ready_on_thread() -> TestResult {
    let (bi, _) = match need_boot_info() {
        Ok(b) => b,
        Err(e) => return ktest::skip(&e),
    };
    let stub = bi.reload_stub().ok_or("no stub")?.phys;
    static RESULT: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
    RESULT.store(0, Ordering::Release);
    let h = sched::spawn("rb-ktest-stub", sched::Class::Normal, move || {
        let r = crate::stage::stub_ready(stub);
        if let Err(e) = &r {
            klog::event!(Warn, "kexec", "ktest.stub_not_ready", error = e.as_str());
        }
        RESULT.store(if r.is_ok() { 1 } else { 2 }, Ordering::Release);
    })
    .map_err(|e| format!("spawn: {e:?}"))?;
    kassert!(h.join_timeout(10 * SEC), "thread did not finish");
    kassert!(RESULT.load(Ordering::Acquire) == 1, "stub not ready from a thread (see kexec ktest.stub_not_ready)");
    Ok(())
}
ktest!("rb.reload.stub_ready_on_thread", Safe, stub_ready_on_thread);

/// Takes frames until only `target` bytes are free (kept for good: the scenario's memory pressure).
fn squeeze_to(key: &str) -> TestResult {
    let Some(mb) = boot::config(key).and_then(|v| v.parse::<u64>().ok()) else {
        return ktest::skip(&format!("CONFIG {key}= not set (only the Reload scenario squeezes memory)"));
    };
    let f = mm_core::frames().ok_or("no frame allocator")?;
    let target = mb << 20;
    let mut taken = 0u64;
    for order in (0..=9u32).rev() {
        while f.stats().free_bytes >= target + (4096u64 << order) {
            if f.alloc(order, mm_core::Zone::Normal, mm_core::Kind::Other).is_none() {
                break;
            }
            taken += 4096 << order;
        }
    }
    let free = f.stats().free_bytes;
    klog::event!(Info, "kexec", "ktest.squeezed", key = key, taken_mb = taken >> 20, free_kb = free >> 10);
    kassert!(free < target + (8 << 10) || taken == 0, "free {free} after squeezing to {target}");
    Ok(())
}
fn squeeze() -> TestResult {
    squeeze_to("rb_squeeze_mb")
}
fn choke() -> TestResult {
    squeeze_to("rb_choke_mb")
}
ktest!("rb.reload.squeeze", QemuOnly, squeeze);
ktest!("rb.reload.choke", QemuOnly, choke);
