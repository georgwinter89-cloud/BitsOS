//! SELFOPT-1 ktests (`so.perf.*`): perf_event_open through the real dispatcher on held test processes (the test
//! thread plays the program, as AB-5's tests do), the ring buffer read straight from the kernel side. Timing is
//! never judged (QEMU): only counts that must exist and fields that must match.

use super::event::{self, Event};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use hal::entry::UserContext;
use ktest::{TestResult, kassert, ktest};
use perf_abi::attr::flag;
use perf_abi::record::{self, Record, Records};
use perf_abi::*;
use proc::Process;
use uapi::errno::*;
use uapi::nr;

pub(super) const SEC: u64 = 1_000_000_000;
pub(super) const MS: u64 = 1_000_000;
pub(super) const EXIT0: [u8; 9] = [0xb8, 0xe7, 0, 0, 0, 0x31, 0xff, 0x0f, 0x05];
/// `jmp $`: a program that only burns its CPU.
pub(super) const SPIN: [u8; 2] = [0xeb, 0xfe];
const SCRATCH: u64 = 1 << 16;
const PERF_EVENT_OPEN: usize = 298;

pub(super) struct T {
    pub(super) p: Arc<Process>,
    th: Arc<proc::Thread>,
    ctx: UserContext,
    pub(super) mem: u64,
    _pin: proc::MmPin,
}

impl T {
    /// A held test process running as root.
    pub(super) fn new(name: &str) -> Result<T, String> {
        T::as_uid(name, 0)
    }
    pub(super) fn as_uid(name: &str, uid: u32) -> Result<T, String> {
        let image = crate::ktests::elf(&EXIT0);
        let argv = [name];
        let mut o = proc::SpawnOptions::new(&argv, &[], policy::ProgramId(1));
        o.held = true;
        o.uid = uid;
        let p = proc::spawn_image_with(name, &image, &o).map_err(|e| format!("spawn {name}: {e}"))?;
        let mem = p.mm.sys_mmap(0, SCRATCH, 3, 0x22, 0).map_err(|e| format!("mmap: {e:?}"))?;
        let th = p.threads().into_iter().next().ok_or("the held process has no main thread")?;
        let _pin = proc::pin_mm(&p);
        Ok(T { p, th, ctx: UserContext::new(0x40_0078, hal::USER_TOP - 4096), mem, _pin })
    }
    pub(super) fn call(&mut self, n: usize, args: &[u64]) -> i64 {
        let mut a = [0u64; 6];
        a[..args.len()].copy_from_slice(args);
        let r = &mut self.ctx.regs;
        (r.rax, r.rdi, r.rsi, r.rdx, r.r10, r.r8, r.r9) = (n as u64, a[0], a[1], a[2], a[3], a[4], a[5]);
        crate::dispatch(&self.p, &self.th, &mut self.ctx);
        self.ctx.regs.rax as i64
    }
    pub(super) fn put(&self, at: u64, b: &[u8]) {
        let _ = self.p.mm.copy_to_user(self.mem + at, b);
    }
    pub(super) fn get(&self, at: u64, len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len];
        let _ = self.p.mm.copy_from_user(&mut v, self.mem + at);
        v
    }
    /// perf_event_open with the attribute `a` at offset 0 of the scratch page.
    pub(super) fn open(&mut self, a: &[u8], pid: i64, cpu: i64, group: i64, flags: u64) -> i64 {
        self.put(0, a);
        let at = self.mem;
        self.call(PERF_EVENT_OPEN, &[at, pid as u64, cpu as u64, group as u64, flags])
    }
    pub(super) fn ev(&self, fd: i64) -> Result<Arc<Event>, String> {
        let f = self.p.fds.get(fd as usize).map_err(|e| format!("fd {fd}: {e}"))?;
        event::of(&f).ok_or_else(|| format!("fd {fd} is no perf event"))
    }
    pub(super) fn close(&mut self, fd: i64) {
        self.call(nr::CLOSE, &[fd as u64]);
    }
    /// read(fd) of `len` bytes into the scratch page: the result and the words.
    pub(super) fn read(&mut self, fd: i64, len: u64) -> (i64, Vec<u64>) {
        let a = self.mem + 0x1000;
        let r = self.call(nr::READ, &[fd as u64, a, len]);
        let n = if r > 0 { r as usize } else { 0 };
        (r, self.get(0x1000, n).chunks(8).map(|c| u64::from_le_bytes(c.try_into().unwrap_or([0; 8]))).collect())
    }
    pub(super) fn ioctl(&mut self, fd: i64, cmd: u64, arg: u64) -> i64 {
        self.call(nr::IOCTL, &[fd as u64, cmd, arg])
    }
}

impl Drop for T {
    fn drop(&mut self) {
        self.p.kill(9);
        let _ = self.p.wait(Some(10 * SEC));
    }
}

/// An attribute of the full size with these fields.
pub(super) struct A {
    pub(super) kind: u32,
    pub(super) config: u64,
    pub(super) sample: u64,
    pub(super) sample_type: u64,
    pub(super) read_format: u64,
    pub(super) flags: u64,
    pub(super) wakeup: u32,
    pub(super) max_stack: u16,
    pub(super) clockid: i32,
}

impl Default for A {
    fn default() -> A {
        A { kind: PERF_TYPE_SOFTWARE, config: SW_CPU_CLOCK, sample: 0, sample_type: 0, read_format: 0, flags: 0, wakeup: 0, max_stack: 0, clockid: 0 }
    }
}

impl A {
    pub(super) fn bytes(&self) -> [u8; attr::SIZE] {
        let mut b = [0u8; attr::SIZE];
        b[0..4].copy_from_slice(&self.kind.to_le_bytes());
        b[4..8].copy_from_slice(&(attr::SIZE as u32).to_le_bytes());
        b[8..16].copy_from_slice(&self.config.to_le_bytes());
        b[16..24].copy_from_slice(&self.sample.to_le_bytes());
        b[24..32].copy_from_slice(&self.sample_type.to_le_bytes());
        b[32..40].copy_from_slice(&self.read_format.to_le_bytes());
        b[40..48].copy_from_slice(&self.flags.to_le_bytes());
        b[48..52].copy_from_slice(&self.wakeup.to_le_bytes());
        b[92..96].copy_from_slice(&self.clockid.to_le_bytes());
        b[108..110].copy_from_slice(&self.max_stack.to_le_bytes());
        b
    }
}

pub(super) fn expect(what: &str, got: i64, want: i64) -> TestResult {
    kassert!(got == want, "{what}: got {got}, want {want}");
    Ok(())
}

/// The records the ring holds between `from` and its head (wrap undone), owned.
pub(super) fn records(ev: &Event, from: u64) -> Result<(Vec<u8>, u64), String> {
    let rb = ev.rb.lock().clone().ok_or("no ring buffer")?;
    let head = rb.head();
    let n = head - from;
    kassert!(n <= rb.data_size(), "head {head} is {n} bytes past {from}, more than the area");
    let mut v = vec![0u8; n as usize];
    rb.read_data(from, &mut v);
    Ok((v, head))
}

/// Every parsed record of `bytes` for event `ev` (owned copies of what the tests look at).
pub(super) fn each(ev: &Event, bytes: &[u8], mut f: impl FnMut(Record<'_>)) -> usize {
    let mut n = 0;
    for r in (Records { bytes, at: 0 }) {
        if let Some(rec) = record::parse(r, ev.attr.sample_type, ev.attr.read_format) {
            f(rec);
            n += 1;
        }
    }
    n
}

pub(super) fn sleep_ms(ms: u64) {
    let end = time::now_ns() + ms * MS;
    while time::now_ns() < end {
        sched::sleep_ns(MS);
    }
}

#[inline(never)]
fn so_perf_busy_spin(ns: u64) -> u64 {
    let end = time::now_ns() + ns;
    let mut x = 0u64;
    while time::now_ns() < end {
        for i in 0..1000u64 {
            x = core::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(i));
        }
    }
    x
}

ktest!("so.perf.attr_checks", Safe, attr_checks);
fn attr_checks() -> TestResult {
    let mut t = T::new("so-perf-attr")?;
    let a = A { flags: flag::EXCLUDE_KERNEL | flag::EXCLUDE_HV, ..Default::default() }.bytes();
    expect("flags outside PERF_FLAG_ALL", t.open(&a, 0, -1, -1, 1 << 4), -EINVAL)?;
    // A size past the page and a longer attribute with non-zero bytes: E2BIG, the kernel's size written back.
    let mut big = [0u8; 160];
    big[..attr::SIZE].copy_from_slice(&a);
    big[4..8].copy_from_slice(&160u32.to_le_bytes());
    big[150] = 1;
    expect("non-zero bytes past the kernel's attribute", t.open(&big, 0, -1, -1, 0), -E2BIG)?;
    expect("size written back", u32::from_le_bytes(t.get(4, 4).try_into().unwrap_or([0; 4])) as i64, attr::SIZE as i64)?;
    let mut small = a;
    small[4..8].copy_from_slice(&40u32.to_le_bytes());
    expect("size below VER0", t.open(&small, 0, -1, -1, 0), -E2BIG)?;
    let mut res = a;
    res[47] = 0x80;
    expect("reserved flag bits", t.open(&res, 0, -1, -1, 0), -EINVAL)?;
    expect("pid -1 and cpu -1", t.open(&a, -1, -1, -1, 0), -EINVAL)?;
    expect("cpu out of range", t.open(&a, 0, 4096, -1, 0), -EINVAL)?;
    let hw = A { kind: PERF_TYPE_HARDWARE, ..Default::default() }.bytes();
    expect("hardware cycles (no PMU driver)", t.open(&hw, 0, -1, -1, 0), -ENOENT)?;
    let pf = A { config: 2, ..Default::default() }.bytes();
    expect("page faults (a software event V2 does not count)", t.open(&pf, 0, -1, -1, 0), -ENOENT)?;
    let freq = A { sample: 200_000, flags: flag::FREQ, ..Default::default() }.bytes();
    expect("frequency above perf_event_max_sample_rate", t.open(&freq, 0, -1, -1, 0), -EINVAL)?;
    let stack = A { sample: 1_000_000, sample_type: SAMPLE_CALLCHAIN, max_stack: 500, ..Default::default() }.bytes();
    expect("sample_max_stack above the sysctl", t.open(&stack, 0, -1, -1, 0), -EOVERFLOW)?;
    let inh = A { flags: flag::INHERIT, ..Default::default() }.bytes();
    expect("inherit (not built)", t.open(&inh, 0, -1, -1, 0), -EOPNOTSUPP)?;
    expect("bad pointer", t.call(PERF_EVENT_OPEN, &[8, 0, u64::MAX, u64::MAX, 0]), -EFAULT)?;
    expect("missing pid", t.open(&a, 4_000_000, -1, -1, 0), -ESRCH)?;
    expect("group fd that is not an event", t.open(&a, 0, -1, 0, 0), -EBADF)?;
    Ok(())
}

ktest!("so.perf.groups", Safe, groups);
fn groups() -> TestResult {
    let mut t = T::new("so-perf-group")?;
    let a = A { read_format: FORMAT_GROUP | FORMAT_ID | FORMAT_TOTAL_TIME_ENABLED, flags: flag::DISABLED, ..Default::default() }.bytes();
    let lead = t.open(&a, -1, 0, -1, 0);
    kassert!(lead >= 0, "leader: {lead}");
    let member = t.open(&a, -1, 0, lead, 0);
    kassert!(member >= 0, "member: {member}");
    expect("a member as leader", t.open(&a, -1, 0, member, 0), -EINVAL)?;
    expect("another target than the leader's", t.open(&a, 0, -1, lead, 0), -EINVAL)?;
    expect("ENABLE with PERF_IOC_FLAG_GROUP", t.ioctl(lead, IOC_ENABLE, IOC_FLAG_GROUP), 0)?;
    sleep_ms(10);
    let ids: Vec<u64> = [lead, member]
        .iter()
        .map(|&fd| {
            t.ioctl(fd, IOC_ID, t.mem + 0x2000);
            u64::from_le_bytes(t.get(0x2000, 8).try_into().unwrap_or([0; 8]))
        })
        .collect();
    expect("group read shorter than the group", t.read(lead, 32).0, -ENOSPC)?;
    let (r, w) = t.read(member, 64);
    // nr, time enabled, (value, id) of the leader, then of the member.
    kassert!(r == 48 && w[0] == 2 && w[3] == ids[0] && w[5] == ids[1], "group read through the member: {r} {w:?} ids {ids:?}");
    kassert!(w[2] > 0 && w[4] > 0, "both members count after a group ENABLE: {w:?}");
    t.close(lead);
    let (r, w) = t.read(member, 64);
    kassert!(r == 32 && w[0] == 1 && w[3] == ids[1], "after the leader's close the member is alone: {r} {w:?}");
    t.close(member);
    Ok(())
}

ktest!("so.perf.permissions", Safe, permissions);
fn permissions() -> TestResult {
    let mut root = T::new("so-perf-root")?;
    let mut t = T::as_uid("so-perf-user", 1000)?;
    let user = A { flags: flag::EXCLUDE_KERNEL | flag::EXCLUDE_HV, ..Default::default() }.bytes();
    let kern = A::default().bytes();
    let chain = A { sample: 1_000_000, sample_type: SAMPLE_IP | SAMPLE_CALLCHAIN, flags: flag::EXCLUDE_KERNEL, ..Default::default() }.bytes();
    let chain_user = A { sample: 1_000_000, sample_type: SAMPLE_IP | SAMPLE_CALLCHAIN, flags: flag::EXCLUDE_KERNEL | flag::EXCLUDE_CALLCHAIN_KERNEL, ..Default::default() }.bytes();
    kassert!(super::PARANOID.load(Ordering::Relaxed) == 2, "perf_event_paranoid is not Linux's default 2");
    expect("unprivileged kernel counting", t.open(&kern, 0, -1, -1, 0), -EACCES)?;
    expect("unprivileged CPU-wide", t.open(&user, -1, 0, -1, 0), -EACCES)?;
    expect("unprivileged kernel callchain", t.open(&chain, 0, -1, -1, 0), -EACCES)?;
    let fd = t.open(&chain_user, 0, -1, -1, 0);
    kassert!(fd >= 0, "unprivileged user-only sampling of itself: {fd}");
    t.close(fd);
    let fd = t.open(&user, 0, -1, -1, 0);
    kassert!(fd >= 0, "unprivileged user-only counting of itself: {fd}");
    t.close(fd);
    // Another user's thread: no ptrace access.
    expect("another user's thread", t.open(&user, root.p.pid as i64, -1, -1, 0), -EACCES)?;
    // Root may do all of it.
    for (what, a, pid, cpu) in [("kernel", &kern, 0, -1), ("CPU-wide", &user, -1, 0), ("kernel chain", &chain, 0, -1), ("other thread", &user, t.p.pid as i64, -1)] {
        let fd = root.open(a, pid, cpu, -1, 0);
        kassert!(fd >= 0, "root {what}: {fd}");
        root.close(fd);
    }
    // paranoid -1 opens CPU-wide events to everyone; kallsyms stays closed by kptr_restrict 1.
    super::PARANOID.store(-1, Ordering::Relaxed);
    let fd = t.open(&user, -1, 0, -1, 0);
    super::PARANOID.store(2, Ordering::Relaxed);
    kassert!(fd >= 0, "CPU-wide at paranoid -1: {fd}");
    t.close(fd);
    kassert!(!super::kallsyms_show(Some(&t.p)), "an unprivileged reader sees kernel addresses");
    kassert!(super::kallsyms_show(Some(&root.p)), "root does not see kernel addresses at kptr_restrict 1");
    Ok(())
}

ktest!("so.perf.counting", Safe, counting);
fn counting() -> TestResult {
    let mut t = T::new("so-perf-count")?;
    let rf = FORMAT_TOTAL_TIME_ENABLED | FORMAT_TOTAL_TIME_RUNNING | FORMAT_ID | FORMAT_LOST;
    let a = A { read_format: rf, flags: flag::DISABLED, ..Default::default() }.bytes();
    let fd = t.open(&a, -1, 0, -1, 0);
    kassert!(fd >= 0, "cpu-clock on CPU 0: {fd}");
    expect("read shorter than read_size", t.read(fd, 16).0, -ENOSPC)?;
    let (r, w) = t.read(fd, 64);
    expect("read size", r, 40)?;
    kassert!(w[0] == 0 && w[1] == 0, "a disabled event counted: {w:?}");
    expect("ioctl ENABLE", t.ioctl(fd, IOC_ENABLE, 0), 0)?;
    sleep_ms(20);
    let (_, w1) = t.read(fd, 64);
    kassert!(w1[0] >= 10 * MS && w1[1] == w1[0] && w1[2] == w1[0], "after 20 ms enabled: {w1:?}");
    expect("ioctl DISABLE", t.ioctl(fd, IOC_DISABLE, 0), 0)?;
    let (_, w2) = t.read(fd, 64);
    sleep_ms(5);
    let (_, w3) = t.read(fd, 64);
    kassert!(w2[0] == w3[0], "a disabled event went on counting: {w2:?} then {w3:?}");
    expect("ioctl RESET", t.ioctl(fd, IOC_RESET, 0), 0)?;
    let (_, w4) = t.read(fd, 64);
    kassert!(w4[0] == 0 && w4[1] >= w3[1], "after RESET: {w4:?} (times go on)");
    expect("ioctl ID", t.ioctl(fd, IOC_ID, t.mem + 0x2000), 0)?;
    let id = u64::from_le_bytes(t.get(0x2000, 8).try_into().unwrap_or([0; 8]));
    kassert!(id != 0 && id == w4[3], "IOC_ID {id} and FORMAT_ID {}", w4[3]);
    expect("unknown ioctl", t.ioctl(fd, 0x2400 | 0x7f, 0), -ENOTTY)?;
    expect("PERIOD on a counting event", t.ioctl(fd, IOC_PERIOD, t.mem + 0x2000), -EINVAL)?;
    // A group read of a group of one.
    let g = A { read_format: FORMAT_GROUP | FORMAT_ID, ..Default::default() }.bytes();
    let gfd = t.open(&g, -1, 0, -1, 0);
    let (r, w) = t.read(gfd, 64);
    kassert!(r == 24 && w[0] == 1, "group read: {r} {w:?}");
    // poll without a buffer: EPOLLHUP (`perf_poll`).
    let f = t.p.fds.get(fd as usize).map_err(|e| format!("{e}"))?;
    kassert!(f.file.events().0 & vfs::poll::Events::HUP != 0, "poll without a buffer is not HUP");
    t.close(gfd);
    t.close(fd);
    Ok(())
}

/// One sampling cpu-clock per CPU at `hz`, with `sample_type`, `pages` data pages each.
fn per_cpu(t: &mut T, hz: u64, sample_type: u64, flags: u64, pages: u64) -> Result<Vec<(i64, Arc<Event>)>, String> {
    let a = A { sample: hz, sample_type, flags: flag::FREQ | flags, ..Default::default() }.bytes();
    let mut v = Vec::new();
    for c in 0..hal::percpu::cpu_count() {
        let fd = t.open(&a, -1, c as i64, -1, 0);
        kassert!(fd >= 0, "cpu-clock on CPU {c}: {fd}");
        let ev = t.ev(fd)?;
        ev.map(0, 1 + pages).map_err(|e| format!("ring of CPU {c}: {e}"))?;
        v.push((fd, ev));
    }
    Ok(v)
}

ktest!("so.perf.sampling_kernel", Safe, sampling_kernel);
fn sampling_kernel() -> TestResult {
    let mut t = T::new("so-perf-kern")?;
    let st = SAMPLE_IP | SAMPLE_TID | SAMPLE_TIME | SAMPLE_CPU | SAMPLE_PERIOD | SAMPLE_CALLCHAIN | SAMPLE_IDENTIFIER;
    let evs = per_cpu(&mut t, 1000, st, 0, 64)?;
    core::hint::black_box(so_perf_busy_spin(300 * MS));
    let me = sched::current_thread().map_or(0, |k| k.id);
    let spin = so_perf_busy_spin as *const () as u64;
    let (mut samples, mut mine, mut in_spin, mut marks) = (0, 0, 0, 0);
    for (fd, ev) in &evs {
        ev.set_enabled(false);
        let (bytes, _) = records(ev, 0)?;
        each(ev, &bytes, |r| {
            if let Record::Sample(s) = r {
                samples += 1;
                if s.id == ev.id && s.period == 1_000_000 && s.cpu as usize == ev.cpu().unwrap_or(usize::MAX) && s.chain_len() >= 2 && s.chain_at(0) == CONTEXT_KERNEL && s.chain_at(1) == s.ip {
                    marks += 1;
                }
                if s.pid == 0 && s.tid == super::sampler::KTHREAD_TID | (me as u32 & !super::sampler::KTHREAD_TID) {
                    mine += 1;
                    if (0..s.chain_len()).any(|i| (spin..spin + 0x200).contains(&s.chain_at(i))) {
                        in_spin += 1;
                    }
                }
            }
        });
        t.close(*fd);
    }
    klog::event!(Info, "ktest", "so.perf.kernel_samples", samples = samples, mine = mine, in_spin = in_spin, marks = marks);
    kassert!(samples >= 100, "only {samples} samples in 300 ms at 1 kHz per CPU");
    kassert!(marks == samples, "{} samples lack the id, period, CPU or kernel mark", samples - marks);
    kassert!(mine >= 50 && in_spin * 2 >= mine, "the busy test thread: {mine} samples, {in_spin} inside so_perf_busy_spin");
    Ok(())
}

ktest!("so.perf.sampling_user", Safe, sampling_user);
fn sampling_user() -> TestResult {
    let mut t = T::new("so-perf-user-opener")?;
    let mut o = proc::SpawnOptions::new(&["so-perf-spin"], &[], policy::ProgramId(1));
    o.held = true;
    let target = proc::spawn_image_with("so-perf-spin", &crate::ktests::elf(&SPIN), &o).map_err(|e| format!("spawn: {e}"))?;
    let st = SAMPLE_IP | SAMPLE_TID | SAMPLE_PERIOD | SAMPLE_CALLCHAIN;
    let a = A { sample: 1000, sample_type: st, flags: flag::FREQ | flag::EXCLUDE_KERNEL, ..Default::default() }.bytes();
    let fd = t.open(&a, target.pid as i64, -1, -1, 0);
    kassert!(fd >= 0, "a thread event on the spinning program: {fd}");
    let ev = t.ev(fd)?;
    ev.map(0, 1 + 16).map_err(|e| format!("ring: {e}"))?;
    target.release();
    sleep_ms(300);
    target.kill(9);
    let _ = target.wait(Some(10 * SEC));
    let (bytes, _) = records(&ev, 0)?;
    let (mut n, mut good) = (0, 0);
    each(&ev, &bytes, |r| {
        if let Record::Sample(s) = r {
            n += 1;
            let user_ip = (0x40_0000..0x40_1000).contains(&s.ip);
            if user_ip && s.misc & MISC_CPUMODE_MASK == MISC_USER && s.pid == target.pid && s.chain_at(0) == CONTEXT_USER && s.chain_at(1) == s.ip {
                good += 1;
            }
        }
    });
    t.close(fd);
    klog::event!(Info, "ktest", "so.perf.user_samples", samples = n, good = good);
    kassert!(n >= 20, "only {n} samples of a program spinning 300 ms at 1 kHz");
    kassert!(good == n, "{} of {n} samples are not the program's user code with its user mark", n - good);
    Ok(())
}

ktest!("so.perf.ring_wrap_lost", Safe, ring_wrap_lost);
fn ring_wrap_lost() -> TestResult {
    let mut t = T::new("so-perf-wrap")?;
    let st = SAMPLE_IP | SAMPLE_TID | SAMPLE_TIME | SAMPLE_CPU | SAMPLE_PERIOD;
    let a = A { sample: 10_000, sample_type: st, read_format: FORMAT_LOST, flags: flag::FREQ | flag::SAMPLE_ID_ALL, ..Default::default() }.bytes();
    let fd = t.open(&a, -1, 0, -1, 0);
    kassert!(fd >= 0, "cpu-clock on CPU 0: {fd}");
    let ev = t.ev(fd)?;
    let rb = ev.map(0, 2).map_err(|e| format!("ring: {e}"))?;
    let size = rb.data_size();
    // Nobody reads: the area fills and the rest is lost.
    sleep_ms(100);
    let full = rb.head();
    kassert!(full > size - 64 && full < size, "a full area stops short of its size: head {full} of {size}");
    let (_, w) = t.read(fd, 16);
    kassert!(w.get(1).is_some_and(|&l| l > 0), "FORMAT_LOST counts no lost samples: {w:?}");
    // The reader takes everything; the next records start with PERF_RECORD_LOST and run over the end of the area.
    rb.set_tail(full);
    sleep_ms(30);
    let (bytes, head) = records(&ev, full)?;
    kassert!(head > full, "nothing new after the reader caught up");
    let (mut first_lost, mut samples, mut n) = (None, 0, 0);
    each(&ev, &bytes, |r| {
        n += 1;
        match r {
            Record::Lost { id, lost } if n == 1 => first_lost = Some((id, lost)),
            Record::Sample(s) if s.cpu == 0 && s.period == 100_000 => samples += 1,
            _ => {}
        }
    });
    let (id, lost) = first_lost.ok_or("the first record after the gap is not PERF_RECORD_LOST")?;
    kassert!(id == ev.id && lost > 0, "LOST record id {id} (event {}), lost {lost}", ev.id);
    kassert!(samples + 1 == n && samples > 5, "{n} records after the gap, {samples} of them samples");
    kassert!((full % size) + bytes.len() as u64 > size, "the records did not run over the end of the area");
    t.close(fd);
    Ok(())
}

ktest!("so.perf.side_band", Safe, side_band);
fn side_band() -> TestResult {
    let mut t = T::new("so-perf-side")?;
    let st = SAMPLE_TID | SAMPLE_TIME;
    let evs = per_cpu(&mut t, 0, st, flag::COMM | flag::MMAP | flag::MMAP2 | flag::TASK | flag::SAMPLE_ID_ALL, 16)?;
    let child = proc::spawn_image("so-perf-child", &crate::ktests::elf(&EXIT0), &["so-perf-child"], &["HOME=/"], policy::ProgramId(1)).map_err(|e| format!("spawn: {e:?}"))?;
    let pid = child.pid;
    let _ = child.wait(Some(10 * SEC));
    sleep_ms(10);
    let (mut comm, mut mmap, mut exit) = (false, false, false);
    for (fd, ev) in &evs {
        let (bytes, _) = records(ev, 0)?;
        each(ev, &bytes, |r| match r {
            Record::Comm { pid: p, tid, name, exec } if p == pid && tid == pid && exec && name == b"so-perf-child" => comm = true,
            Record::Mmap2 { pid: p, addr, prot, name, .. } if p == pid && addr <= 0x40_0000 && prot & 4 != 0 && !name.is_empty() => mmap = true,
            Record::Exit { pid: p, tid, .. } if p == pid && tid == pid => exit = true,
            _ => {}
        });
        t.close(*fd);
    }
    kassert!(comm, "no COMM (exec) record of the new program");
    kassert!(mmap, "no MMAP2 record of its code");
    kassert!(exit, "no EXIT record when it ended");
    Ok(())
}

ktest!("so.perf.hup_and_poll", Safe, hup_and_poll);
fn hup_and_poll() -> TestResult {
    let mut t = T::new("so-perf-poll")?;
    let mut o = proc::SpawnOptions::new(&["so-perf-short"], &[], policy::ProgramId(1));
    o.held = true;
    let target = proc::spawn_image_with("so-perf-short", &crate::ktests::elf(&EXIT0), &o).map_err(|e| format!("spawn: {e}"))?;
    let a = A { flags: flag::EXCLUDE_KERNEL, ..Default::default() }.bytes();
    let fd = t.open(&a, target.pid as i64, -1, -1, 0);
    kassert!(fd >= 0, "a thread event: {fd}");
    let f = t.p.fds.get(fd as usize).map_err(|e| format!("{e}"))?;
    target.release();
    let _ = target.wait(Some(10 * SEC));
    let ev = t.ev(fd)?;
    kassert!(ev.hup.load(Ordering::Acquire), "the event did not see its thread end");
    kassert!(f.file.events().0 & vfs::poll::Events::HUP != 0, "poll after the end is not HUP");
    t.close(fd);
    // A buffer's wake-up: POLLIN once, then nothing until the next one.
    let s = A { sample: 1000, sample_type: SAMPLE_IP, flags: flag::FREQ, wakeup: 1, ..Default::default() }.bytes();
    let fd = t.open(&s, -1, 0, -1, 0);
    let ev = t.ev(fd)?;
    ev.map(0, 1 + 4).map_err(|e| format!("ring: {e}"))?;
    sleep_ms(20);
    let f = t.p.fds.get(fd as usize).map_err(|e| format!("{e}"))?;
    ev.set_enabled(false);
    let first = f.file.events().0;
    let second = f.file.events().0;
    kassert!(first & vfs::poll::Events::IN != 0 && second == 0, "poll gave {first:#x} then {second:#x}");
    t.close(fd);
    Ok(())
}

ktest!("so.perf.mmap_syscall", Safe, mmap_syscall);
fn mmap_syscall() -> TestResult {
    let mut t = T::new("so-perf-mmap")?;
    let a = A { sample: 1000, sample_type: SAMPLE_IP, flags: flag::FREQ | flag::DISABLED, ..Default::default() }.bytes();
    let fd = t.open(&a, -1, 0, -1, 0);
    kassert!(fd >= 0, "cpu-clock: {fd}");
    let mm = |t: &mut T, pages: u64, flags: u64| t.call(nr::MMAP, &[0, pages * 4096, 3, flags, fd as u64, 0]);
    expect("1 + 3 pages (3 is no power of two)", mm(&mut t, 4, 1), -EINVAL)?;
    expect("MAP_PRIVATE", mm(&mut t, 3, 2), -EINVAL)?;
    let at = mm(&mut t, 3, 1);
    kassert!(at > 0, "1 + 2 pages MAP_SHARED: {at}");
    expect("a second size", mm(&mut t, 5, 1), -EINVAL)?;
    let again = mm(&mut t, 3, 1);
    kassert!(again > 0 && again != at, "the same size again: {again}");
    let mut page = [0u8; 1056];
    t.p.mm.copy_from_user(&mut page, at as u64).map_err(|_| String::from("the control page cannot be read"))?;
    let u32_at = |o: usize| u32::from_le_bytes(page[o..o + 4].try_into().unwrap_or([0; 4]));
    let u64_at = |o: usize| u64::from_le_bytes(page[o..o + 8].try_into().unwrap_or([0; 8]));
    kassert!(u32_at(page::VERSION) == 0 && u32_at(page::SIZE) == page::SIZE_VALUE, "version/size {} {}", u32_at(0), u32_at(page::SIZE));
    kassert!(u64_at(page::DATA_OFFSET) == 4096 && u64_at(page::DATA_SIZE) == 8192, "data offset/size {} {}", u64_at(page::DATA_OFFSET), u64_at(page::DATA_SIZE));
    kassert!(u64_at(page::CAPABILITIES) & page::CAP_BIT0_IS_DEPRECATED != 0, "cap_bit0_is_deprecated");
    // The program writes data_tail through its mapping; the kernel sees it.
    let ev = t.ev(fd)?;
    t.p.mm.copy_to_user(at as u64 + page::DATA_TAIL as u64, &64u64.to_le_bytes()).map_err(|_| String::from("data_tail cannot be written"))?;
    let rb = ev.rb.lock().clone().ok_or("no ring after mmap")?;
    kassert!(rb.page_u64(page::DATA_TAIL) == 64, "the kernel does not see the program's data_tail");
    t.close(fd);
    Ok(())
}

ktest!("so.perf.procfs", Safe, procfs);
fn procfs() -> TestResult {
    let ns = vfs::root_ns();
    for (f, want) in [("perf_event_paranoid", "2\n"), ("kptr_restrict", "1\n"), ("perf_event_max_sample_rate", "100000\n"), ("perf_event_mlock_kb", "516\n"), ("perf_event_max_stack", "127\n")] {
        let got = ns.read_file(&format!("/proc/sys/kernel/{f}"), 64).map_err(|e| format!("{f}: {e}"))?;
        kassert!(got == want.as_bytes(), "{f}: {:?}", String::from_utf8_lossy(&got));
    }
    let ks = ns.read_file("/proc/kallsyms", 16 << 20).map_err(|e| format!("/proc/kallsyms: {e}"))?;
    let text = String::from_utf8_lossy(&ks);
    let Some((t, _)) = kexec::crash::symbols() else {
        return ktest::fail("this boot has no kernel symbol table (boot/kernel.syms); /proc/kallsyms cannot be checked");
    };
    kassert!(text.lines().count() == t.len(), "{} lines for {} symbols", text.lines().count(), t.len());
    let me = format!("{:016x} t ", so_perf_busy_spin as *const () as u64);
    kassert!(text.lines().any(|l| l.starts_with(&me) && l.contains("so_perf_busy_spin")), "this test's function is not in /proc/kallsyms at its address");
    let c = ns.read_file("/proc/bitsos/perf", 4096).map_err(|e| format!("/proc/bitsos/perf: {e}"))?;
    let c = String::from_utf8_lossy(&c);
    for k in ["events_opened ", "samples ", "records_lost ", "sampler_ticks ", "sampler_ns "] {
        kassert!(c.lines().any(|l| l.starts_with(k) && l[k.len()..].parse::<u64>().is_ok()), "/proc/bitsos/perf has no {k}line:\n{c}");
    }
    Ok(())
}

// The profiler's own cost so far (the sampler's interrupt time), for the notes.
ktest!("so.perf.cost_counters", Safe, cost_counters);
fn cost_counters() -> TestResult {
    let mut t = T::new("so-perf-cost")?;
    let (n0, ns0) = super::sampler::cost();
    let evs = per_cpu(&mut t, 1000, SAMPLE_IP | SAMPLE_TID | SAMPLE_TIME | SAMPLE_CPU | SAMPLE_PERIOD | SAMPLE_CALLCHAIN, 0, 16)?;
    kassert!(super::sampler::running(0), "the sampler of CPU 0 does not run");
    sleep_ms(200);
    for (fd, _) in &evs {
        t.close(*fd);
    }
    kassert!(!super::sampler::running(0), "the sampler of CPU 0 still runs after the last close");
    let (n1, ns1) = super::sampler::cost();
    let (ticks, ns) = (n1 - n0, ns1 - ns0);
    kassert!(ticks > 100, "only {ticks} sampler interrupts in 200 ms");
    klog::event!(Info, "ktest", "so.perf.cost", ticks = ticks, ns_per_tick = ns / ticks.max(1), cpus = hal::percpu::cpu_count());
    Ok(())
}
