//! AB-14: the vDSO (Linux's x86-64 vDSO, linux-6.6/arch/x86/entry/vdso/vma.c for the mapping and
//! kernel/time/vsyscall.c for the data; own code). Every program gets two mappings from exec on, as Linux
//! places them: `[vvar]`, one read-only page the kernel keeps current (`layout.rs`), and right after it
//! `[vdso]`, the ELF image `build.rs` makes from `../vdso/vdso.rs` (read+exec). AT_SYSINFO_EHDR names the image;
//! glibc and musl find `__vdso_clock_gettime` & co. there (version LINUX_2.6) and stop making those syscalls.
//!
//! Both are one device mapping (`mm::device`, MM-5b) over a write-back DMA buffer: the frames are shared by all
//! processes, never copied on fork or written by a program (the pseudo fd is read-only, so even mprotect cannot
//! add PROT_WRITE; Linux lets gdb COW the text, V2 answers EACCES there), and they stay for the kernel's
//! life. Placement: where mmap finds room top-down below the mmap base after the program and its interpreter,
//! the slot Linux's `get_unmapped_area` gives the vDSO with randomization off (V2 randomizes no mapping:
//! `randomize_va_space` is 0).
//!
//! The data: the time crate's clock (TSC since boot, the same `ns = base_ns + (tsc - base_tsc) * mult >> 32`
//! as `time::clock::Snapshot::ns_at`) plus the wall clock's offset, published with a sequence count. `refresh`
//! republishes when the rate, the wall offset or the CPU facts changed; `proc.timers` calls it at least once a
//! second as a safety net; the time crate calls `publish` itself at every rate change and wall-clock set
//! (`time::set_publish_hook`, INT-W19), and the base is `time::snapshot()`'s own line. Outside the ktests the rate changes only at calibration (acpi boot) and
//! at the Reload re-check one second later; until both are done the page says MODE_NONE, so a page with an old
//! rate never answers and CLOCK_MONOTONIC never steps back when the page catches up.

pub mod layout;

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU8, AtomicU32, Ordering, compiler_fence};
use layout::*;
use mm::device::{Aperture, DeviceMap, DeviceOps, DevicePage};
use mm::dma::{DmaBuffer, DmaConstraints};
use mm::{AddressSpace, FileRef};
use vfs::pagecache::PageFault;
use vfs::{Errno, OpenFile};

/// The image (checked by build.rs: one PT_LOAD at 0, no relocations).
static IMAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/vdso.so"));

const PAGE: u64 = 4096;
const PROT_READ: u64 = 1;
const PROT_WRITE: u64 = 2;
const PROT_EXEC: u64 = 4;
const MAP_PRIVATE: u64 = 2;

klog::counter!(PUBLISHED, "proc.vdso.published");

/// The image's size in pages (the file rounded up).
pub fn image_pages() -> u64 {
    (IMAGE.len() as u64).div_ceil(PAGE)
}

/// The image's bytes (ktests compare the mapping with them).
pub fn image() -> &'static [u8] {
    IMAGE
}

/// vvar page 0, then the image pages; nothing a program may write.
struct Pages {
    pages: u64,
}

impl DeviceOps for Pages {
    fn check(&self, pgoff: u64, pages: u64, prot: u64, _shared: bool) -> Result<(), i64> {
        if prot & PROT_WRITE != 0 {
            return Err(uapi::errno::EACCES);
        }
        if pgoff.checked_add(pages).is_none_or(|e| e > self.pages) {
            return Err(uapi::errno::EINVAL);
        }
        Ok(())
    }
    fn fault(&self, pgoff: u64, _write: bool) -> Result<DevicePage, PageFault> {
        if pgoff >= self.pages {
            return Err(PageFault::Bus);
        }
        Ok(DevicePage { aperture: 0, page: pgoff, writable: false })
    }
}

/// The pseudo file behind the mapping (no name, never read: `mm::device::check` wants an fd open for reading).
struct NoFile;

impl vfs::File for NoFile {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(uapi::errno::EINVAL)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(uapi::errno::EINVAL)
    }
    fn mode(&self) -> u32 {
        0o100400 // S_IFREG | 0400: never seen (the fd is nobody's)
    }
}

struct Vdso {
    buf: Arc<DmaBuffer>,
    map: Arc<DeviceMap>,
    file: Arc<OpenFile>,
}

static VDSO: ksync::Once<Option<Vdso>> = ksync::Once::new();
/// The writers' own sequence count (the page's `SEQ` mirrors it): even = free, odd = someone writes.
static WSEQ: AtomicU32 = AtomicU32::new(0);
/// ktests: 0 = the TSC rule, 1 = TSC marked unusable, 2 = as the rule.
static TSC_OVERRIDE: AtomicU8 = AtomicU8::new(0);
/// Publishers (`publish`, `refresh`, the ktests' hold) work one at a time: each reads the clock and writes it
/// under this lock, so an older reading never lands over a newer one. Taken before the time crate's locks
/// (`current` reads the wall clock); only thread context publishes.
static PUB: ksync::Mutex<()> = ksync::Mutex::named("proc.vdso.publish", ());

fn make() -> Option<Vdso> {
    let pages = 1 + image_pages();
    let buf = match DmaBuffer::alloc((pages * PAGE) as usize, DmaConstraints::dma64()) {
        Ok(b) => Arc::new(b),
        Err(e) => {
            klog::event!(Error, "proc", "vdso.no_memory", error = alloc::format!("{e:?}"); "no vDSO: programs make the clock syscalls");
            return None;
        }
    };
    if !buf.write(PAGE as usize, IMAGE) {
        klog::event!(Error, "proc", "vdso.copy_failed"; "no vDSO");
        return None;
    }
    let map = match DeviceMap::new(Arc::new(Pages { pages }), alloc::vec![Aperture::Ram(buf.clone())]) {
        Ok(m) => m,
        Err(e) => {
            klog::event!(Error, "proc", "vdso.no_map", error = alloc::format!("{e:?}"); "no vDSO");
            return None;
        }
    };
    let file = OpenFile::anon(Arc::new(NoFile), vfs::abi::O_RDONLY);
    klog::event!(Info, "proc", "vdso.ready", image_bytes = IMAGE.len(), pages = pages);
    Some(Vdso { buf, map, file })
}

fn get() -> Option<&'static Vdso> {
    let mut first = false;
    let v = VDSO.get_or_init(|| {
        first = true;
        make()
    });
    if first && v.is_some() {
        // INT-W19 (AB-14 CR 1): from now on the time crate publishes at every rate change and wall-clock set;
        // `refresh` stays as the once-a-second safety net (CPU facts, and a hook another caller took first).
        if !time::set_publish_hook(publish) {
            klog::event!(Warn, "proc", "vdso.hook_taken"; "the time crate's publish hook is already set; the vDSO page follows the clock by refresh only");
        }
        publish();
    }
    v.as_ref()
}

/// Makes the page and the image now if no exec has yet (ktests that use the page before any program ran).
pub fn ensure() -> bool {
    get().is_some()
}

/// Whether `f` is the vDSO's mapping (`/proc/<pid>/maps` names it `[vvar]` at file page 0, else `[vdso]`).
pub fn is_vdso(f: &FileRef) -> bool {
    match (VDSO.get().and_then(|v| v.as_ref()), f.device.as_ref()) {
        (Some(v), Some(d)) => Arc::ptr_eq(&v.map, d),
        _ => false,
    }
}

/// Maps `[vvar]` and `[vdso]` into `mm` (exec): the image's address for AT_SYSINFO_EHDR, None (logged) when
/// there is no vDSO or no room; the program then runs without one, as on Linux with `vdso=0`.
pub(crate) fn map_into(mm: &AddressSpace) -> Option<u64> {
    let v = get()?;
    let text = image_pages() * PAGE;
    let at = match mm.sys_mmap_device(0, PAGE + text, PROT_READ, MAP_PRIVATE, 0, &v.file, &v.map) {
        Ok(a) => a,
        Err(e) => {
            klog::event!(Warn, "proc", "vdso.map_failed", errno = e; "this program runs without a vDSO");
            return None;
        }
    };
    if let Err(e) = mm.sys_mprotect(at + PAGE, text, PROT_READ | PROT_EXEC) {
        let _ = mm.sys_munmap(at, PAGE + text);
        klog::event!(Warn, "proc", "vdso.protect_failed", error = alloc::format!("{e:?}"); "this program runs without a vDSO");
        return None;
    }
    Some(at + PAGE)
}

/// What the page says (all but the sequence count).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    pub mode: u32,
    pub base_tsc: u64,
    pub base_ns: u64,
    pub mult: u64,
    pub real_off: i64,
    pub flags: u32,
}

impl Params {
    /// The same clock (`base_*` may differ: any point of the line is as good).
    fn same_clock(&self, o: &Params) -> bool {
        self.mode == o.mode && self.mult == o.mult && self.real_off == o.real_off && self.flags == o.flags
    }
}

/// CLOCK_REALTIME minus CLOCK_MONOTONIC from the wall clock's base alone (Linux `offs_real`): the same value
/// every time until the base changes. Matches `time::wall::realtime_ns` (0 without a wall time).
pub fn realtime_offset_ns() -> i64 {
    use time::wall::Base;
    let (base, tz, ..) = time::wall::state();
    let off: i128 = match base {
        Base::None => 0,
        Base::Rtc { local, mono_ns } => match u64::try_from(tz.to_utc(&local)) {
            Ok(s) => s.saturating_mul(1_000_000_000) as i128 - mono_ns as i128,
            Err(_) => 0,
        },
        Base::Set { utc_ns, mono_ns } => utc_ns as i128 - mono_ns as i128,
    };
    off.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

/// Whether the vDSO may read the TSC itself. V2's clock is the TSC everywhere (the syscalls read the same
/// counter on the same CPU), so the vDSO answers whenever the clock runs: a rate is set and the TSC is invariant
/// or a hypervisor presents it (QEMU/KVM without `invtsc`: the kernel's own clock reads that TSC too, so the
/// answers stay equal). Every x86-64-v3 CPU has an invariant TSC; ktests mark it unusable to test the fallback.
fn tsc_usable(hz: u64, settled: bool) -> bool {
    match TSC_OVERRIDE.load(Ordering::Acquire) {
        1 => return false,
        2 => return hz != 0,
        _ => {}
    }
    let f = hal::cpu::live();
    settled && hz != 0 && (f.has("constant_tsc") || f.has("hypervisor"))
}

/// The clock now, as the page should say it.
pub fn current() -> Params {
    // Read before the rate: the re-check stores "done" after its rate change (Release), so a rate read after
    // "done" is the final one.
    let settled = !time::recheck_pending() && time::calibration().is_some();
    // INT-W19 (AB-14 CR 1): the clock's own line (base, rate and multiplier from one snapshot), so the page and
    // `time::now_ns` compute the very same value from every TSC.
    let snap = time::snapshot();
    let (hz, base_tsc, base_ns) = (snap.hz, snap.base_tsc, snap.base_ns);
    let f = hal::cpu::live();
    let (rdtscp, rdpid) = (f.has("rdtscp"), f.has("rdpid"));
    let cpus = hal::percpu::cpu_count();
    let mut flags = 0;
    if rdtscp {
        flags |= F_RDTSCP;
    }
    if rdpid {
        flags |= F_RDPID;
    }
    if (rdtscp || rdpid) && cpus > 0 && hal::cpu::tsc_aux_cpu_fits(cpus as u32 - 1) {
        flags |= F_GETCPU;
    }
    let mode = if tsc_usable(hz, settled) { MODE_TSC } else { MODE_NONE };
    Params { mode, base_tsc, base_ns, mult: snap.ns_mult, real_off: realtime_offset_ns(), flags }
}

/// The page's contents now (ktests).
pub fn published() -> Option<(u32, Params)> {
    let b = &VDSO.get()?.as_ref()?.buf;
    let r32 = |o| b.read_u32(o).unwrap_or(0);
    let r64 = |o| b.read_u64(o).unwrap_or(0);
    Some((r32(SEQ), Params { mode: r32(MODE), base_tsc: r64(BASE_TSC), base_ns: r64(BASE_NS), mult: r64(MULT), real_off: r64(REAL_OFF) as i64, flags: r32(FLAGS) }))
}

/// Writes `p` into the page under the sequence count. Writers serialise on `WSEQ`; interrupts stay off while
/// the count is odd, so readers never wait for a preempted writer.
pub fn write(p: &Params) {
    let Some(v) = VDSO.get().and_then(|v| v.as_ref()) else { return };
    let b = &v.buf;
    let _irq = hal::cpu::IrqGuard::new();
    let s = begin(b);
    b.write_u32(MODE, p.mode);
    b.write_u64(BASE_TSC, p.base_tsc);
    b.write_u64(BASE_NS, p.base_ns);
    b.write_u64(MULT, p.mult);
    b.write_u64(REAL_OFF, p.real_off as u64);
    b.write_u64(TAI_OFF, 0);
    b.write_u64(BOOT_OFF, 0);
    // Every V2 clock is as exact as the TSC (the COARSE ones too): 1 ns, as clock_getres answers.
    b.write_u32(RES_NS, 1);
    b.write_u32(COARSE_RES_NS, 1);
    b.write_u32(FLAGS, p.flags);
    end(b, s);
    PUBLISHED.inc();
}

/// Takes the writers' count (even `s` becomes `s + 1`) and makes the page's count odd: readers retry from now.
fn begin(b: &DmaBuffer) -> u32 {
    let mut s = WSEQ.load(Ordering::Relaxed);
    loop {
        if s & 1 == 0 {
            match WSEQ.compare_exchange_weak(s, s.wrapping_add(1), Ordering::Acquire, Ordering::Relaxed) {
                Ok(_) => break,
                Err(x) => s = x,
            }
        } else {
            core::hint::spin_loop();
            s = WSEQ.load(Ordering::Relaxed);
        }
    }
    b.write_u32(SEQ, s.wrapping_add(1));
    compiler_fence(Ordering::SeqCst);
    s
}

/// Ends what `begin` returned `s` for: the page's count even again, then the writers'.
fn end(b: &DmaBuffer, s: u32) {
    compiler_fence(Ordering::SeqCst);
    b.write_u32(SEQ, s.wrapping_add(2));
    WSEQ.store(s.wrapping_add(2), Ordering::Release);
}

/// Publishes the clock now (the time crate's hook once it has one: every rate change and wall-clock set).
pub fn publish() {
    publish_after(0);
}

/// `publish` with a sleep of `wait_ns` between reading the clock and writing the page (0 in the kernel; the
/// ktests' stand-in for a publisher preempted there).
fn publish_after(wait_ns: u64) {
    let _g = PUB.lock();
    let p = current();
    if wait_ns != 0 {
        sched::sleep_ns(wait_ns);
    }
    write(&p);
    klog::event!(Debug, "proc", "vdso.published", mode = p.mode, mult = p.mult, real_off = p.real_off, flags = p.flags);
}

/// ktests: a publisher that reads the clock, then sleeps `wait_ns` before it writes (see `publish_after`).
pub fn publish_slow_for_test(wait_ns: u64) {
    publish_after(wait_ns);
}

/// ktests: runs `f` with the page's count held odd (a writer stuck mid-update): readers must fall back to the
/// syscall. Other publishers wait on `PUB` meanwhile; interrupts stay on. The page is republished after.
pub fn hold_odd_for_test<R>(f: impl FnOnce() -> R) -> Option<R> {
    let b = &VDSO.get()?.as_ref()?.buf;
    let r = {
        let _g = PUB.lock();
        let s = begin(b);
        let r = f();
        end(b, s);
        r
    };
    publish();
    Some(r)
}

/// Publishes when the clock changed since the last time (rate, wall offset, mode, CPU facts).
pub fn refresh() {
    if VDSO.get().and_then(|v| v.as_ref()).is_none() {
        return;
    }
    let _g = PUB.lock();
    let now = current();
    match published() {
        Some((_, old)) if old.same_clock(&now) => {}
        Some((_, old)) => {
            write(&now);
            klog::event!(Info, "proc", "vdso.republished", old_mult = old.mult, mult = now.mult, old_real_off = old.real_off, real_off = now.real_off,
                old_mode = old.mode, mode = now.mode; "the clock changed under the vDSO; programs read the new one from now on");
        }
        None => {}
    }
}

/// ktests: Some(false) marks the TSC unusable for the vDSO (every call makes the syscall), Some(true) lets it
/// answer, None restores the rule; publishes at once.
pub fn set_tsc_usable_for_test(v: Option<bool>) {
    TSC_OVERRIDE.store(
        match v {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        },
        Ordering::Release,
    );
    if get().is_some() {
        publish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn image_is_a_small_shared_object_with_linux_names() {
        assert_eq!(&IMAGE[..4], b"\x7fELF");
        assert_eq!(u16::from_le_bytes([IMAGE[16], IMAGE[17]]), 3, "ET_DYN");
        assert!((1..=2).contains(&image_pages()), "{} pages", image_pages());
        for n in ["__vdso_clock_gettime", "__vdso_gettimeofday", "__vdso_time", "__vdso_getcpu", "__vdso_clock_getres", "LINUX_2.6", "linux-vdso.so.1", "Linux"] {
            assert!(has(IMAGE, n.as_bytes()), "{n} missing");
        }
    }

    #[test]
    fn layout_fits_the_page() {
        assert!(FLAGS + 4 <= TIMENS && TIMENS < VVAR_SIZE);
        for (o, w) in [(SEQ, 4), (MODE, 4), (BASE_TSC, 8), (BASE_NS, 8), (MULT, 8), (REAL_OFF, 8), (TAI_OFF, 8), (BOOT_OFF, 8), (RES_NS, 4), (COARSE_RES_NS, 4), (FLAGS, 4)] {
            assert_eq!(o % w, 0, "field at {o} not aligned");
        }
    }
}
