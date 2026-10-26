//! In-kernel tests of the mirror: framing, the early buffer (a private mirror fed from boot), the emergency path
//! on a fake card with interrupts off, and, when CONFIG names a listener (the scenario
//! `host/tests/rb/30-mirror.sh`, or the Mini-PC with Georg's `mirror=`), the live mirror and an emergency line
//! through `poll_tx` (both Safe: one frame to the configured listener) and, under QEMU only, a forced panic.

use super::wire::{self, DGRAM_MAX, FRAME_HEAD, FRAME_MAX, FrameAddr, HEAD, Kind, Target};
use super::{Mirror, RouteInfo};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use devmodel::DevError;
use devmodel::net::{LinkState, MacAddr, NetDevice};
use ksync::SpinLock;
use ktest::{TestResult, kassert, ktest};

const TARGET: Target = Target { ip: [10, 0, 2, 2], port: 23300 };

/// A card that keeps what `poll_tx` hands it (and refuses when told to).
pub struct FakeCard {
    pub frames: SpinLock<Vec<Vec<u8>>>,
    pub refuse: core::sync::atomic::AtomicBool,
}

impl FakeCard {
    pub fn new() -> Arc<FakeCard> {
        Arc::new(FakeCard { frames: SpinLock::new(Vec::new()), refuse: core::sync::atomic::AtomicBool::new(false) })
    }
    /// The datagrams of every frame, each checked (checksums, addresses).
    pub fn datagrams(&self, src_ip: [u8; 4]) -> Result<Vec<Vec<u8>>, String> {
        let mut out = Vec::new();
        for f in self.frames.lock().iter() {
            let (a, d) = wire::parse_frame(f).ok_or("a frame with a bad checksum or header")?;
            if a.dst_ip != TARGET.ip || a.dst_port != TARGET.port || a.src_ip != src_ip || a.dst_mac != wire::ETH_BROADCAST {
                return Err(alloc::format!("frame addressed wrongly: {a:?}"));
            }
            out.push(d.to_vec());
        }
        Ok(out)
    }
}

impl NetDevice for FakeCard {
    fn name(&self) -> &str {
        "fake-mirror0"
    }
    fn mac(&self) -> MacAddr {
        MacAddr([2, 0, 0, 0, 0, 1])
    }
    fn link(&self) -> LinkState {
        LinkState { up: true, speed_mbps: 1000, full_duplex: true }
    }
    fn transmit(&self, _frame: &[u8]) -> Result<(), DevError> {
        Err(DevError::Refused)
    }
    fn receive(&self, _buf: &mut [u8]) -> Option<usize> {
        None
    }
    fn poll_tx(&self, frame: &[u8]) -> bool {
        if self.refuse.load(core::sync::atomic::Ordering::Acquire) {
            return false;
        }
        self.frames.lock().push(frame.to_vec());
        true
    }
}

pub fn fake_route(card: &Arc<FakeCard>) -> RouteInfo {
    RouteInfo { card: card.clone(), src_ip: [10, 0, 2, 15], src_mac: [2, 0, 0, 0, 0, 1], dst_mac: wire::ETH_BROADCAST }
}

/// Joins the text of `Log` datagrams, checking that each starts where the previous ended (from `start`).
pub fn log_text(dgrams: &[Vec<u8>], id: u32, start: u64) -> Result<Vec<u8>, String> {
    let mut text = Vec::new();
    let mut next = start;
    for d in dgrams {
        let h = wire::parse_head(d).ok_or("not a mirror datagram")?;
        if h.kind != Kind::Log {
            continue;
        }
        if h.id != id {
            return Err(alloc::format!("id {:#x}, want {id:#x}", h.id));
        }
        let body = &d[HEAD..];
        if h.pos > next {
            return Err(alloc::format!("gap: datagram at {} after {}", h.pos, next));
        }
        let skip = (next - h.pos) as usize;
        if skip < body.len() {
            text.extend_from_slice(&body[skip..]);
            next = h.pos + body.len() as u64;
        }
    }
    Ok(text)
}

fn encoded(s: &str) -> Vec<u8> {
    let mut v = Vec::new();
    wire::encode(s, |b| v.push(b));
    v
}

static EARLY: ksync::OnceRef<Mirror> = ksync::OnceRef::new();
static FAKE: ksync::OnceRef<Mirror> = ksync::OnceRef::new();

/// One private mirror per test and boot (a sink must be 'static): made on the first run, reset on later ones,
/// so `ktest` over MCP can repeat these Safe tests on the Mini-PC without leaking a ring each time.
fn test_mirror(slot: &'static ksync::OnceRef<Mirror>, bytes: usize) -> Result<&'static Mirror, String> {
    if slot.get().is_none() {
        let m = Mirror::new(TARGET, bytes, time::now_ns).ok_or("no memory for a test mirror")?;
        slot.set(alloc::boxed::Box::leak(alloc::boxed::Box::new(m)));
    }
    let m = slot.get().ok_or("test mirror")?;
    m.reset();
    Ok(m)
}

fn framing() -> TestResult {
    let mut f = [0u8; FRAME_MAX];
    let a = FrameAddr { src_mac: [2, 0, 0, 0, 0, 1], dst_mac: wire::ETH_BROADCAST, src_ip: [10, 0, 2, 15], dst_ip: TARGET.ip, src_port: 6666, dst_port: TARGET.port };
    let n = wire::text(&mut f[FRAME_HEAD..], Kind::Log, 0x1234_5678, 99, b"line one\n").ok_or("text")?;
    let fl = wire::frame(&mut f, &a, 7, n).ok_or("frame")?;
    kassert!(fl == 60 + 7, "{} bytes", fl);
    // NW's own RFC 1071 sum over the IPv4 header (an independent implementation) must come out 0.
    kassert!(net::frames::checksum(&f[14..34]) == 0, "IPv4 header checksum");
    let (got, d) = wire::parse_frame(&f[..fl]).ok_or("frame does not parse")?;
    kassert!(got == a);
    kassert!(&d[..4] == b"BM\x01L" && d[HEAD..] == *b"line one\n", "{:?}", d);
    kassert!(wire::parse_head(d) == Some(wire::Head { kind: Kind::Log, id: 0x1234_5678, pos: 99 }));
    let n = wire::begin(&mut f[FRAME_HEAD..], 1, 5, 2, [10, 0, 2, 15]).ok_or("begin")?;
    let fl = wire::frame(&mut f, &a, 8, n).ok_or("frame")?;
    let (_, d) = wire::parse_frame(&f[..fl]).ok_or("B frame")?;
    kassert!(d.len() == HEAD + 9 && d[3] == b'B' && d[HEAD..HEAD + 4] == [2, 0, 0, 0] && d[HEAD + 4..HEAD + 8] == [10, 0, 2, 15]);
    let big = [b'x'; 3000];
    let n = wire::text(&mut f[FRAME_HEAD..], Kind::Log, 1, 0, &big).ok_or("text")?;
    let fl = wire::frame(&mut f, &a, 9, n).ok_or("frame")?;
    kassert!(fl == 1458 && fl <= 1514, "a full datagram fits an Ethernet frame: {}", fl);
    kassert!(encoded("Größe → ok") == b"Gr\x94\xe1e ? ok", "CP437");
    Ok(())
}
ktest!("rb.mirror.framing", Safe, framing);

/// A private mirror registered like the kernel's (deferred sink with replay) holds the boot from its first
/// record: its ring starts with exactly the records klog kept, rendered, one line each. (Its ring is as large
/// as klog's, so the ktests logged before this one cannot push the boot out of it; the kernel's own ring is
/// sized from memory, checked by `rb.mirror.sized`.)
fn early_buffer() -> TestResult {
    let m = test_mirror(&EARLY, klog::ring_info().bytes.max(64 << 10))?;
    if m.ring_state().4 < klog::ring_info().bytes {
        return ktest::skip("klog's ring grew since the first run in this boot");
    }
    let id = klog::add_deferred_sink("ktest-mirror", m, true);
    let mark = alloc::format!("{}", time::now_ns());
    klog::event!(Error, "rb", "mirror.ktest_mark", mark = mark.as_str());
    let deadline = time::now_ns() + 10_000_000_000;
    let want = encoded(&alloc::format!("mirror.ktest_mark mark={mark}"));
    let mut found = false;
    while time::now_ns() < deadline && !found {
        sched::sleep_ms(5);
        found = m.ring_text(0, usize::MAX).windows(want.len()).any(|w| w == want.as_slice());
    }
    klog::remove_sink(id);
    kassert!(found, "the marker never reached the mirror's ring");
    let (_, sent, oldest, _, _) = m.ring_state();
    kassert!(sent == 0, "a mirror that is not live sends nothing (R9 until the network): sent={}", sent);
    kassert!(oldest == 0, "the private ring overflowed");
    // Compare with klog's own records from the oldest it still holds.
    let page = klog::read(0, 64, &klog::Filter::ALL);
    let first = page.records.first().ok_or("klog is empty")?;
    if first.seq != 0 || !page.gaps.is_empty() {
        return ktest::skip("klog's own ring no longer holds the start of the boot");
    }
    let mut expect = Vec::new();
    for r in &page.records {
        expect.extend(encoded(&r.render()));
        expect.push(b'\n');
    }
    let got = m.ring_text(0, expect.len());
    kassert!(
        got == expect,
        "the mirror's first {} bytes differ from klog's first {} records: {:?}",
        expect.len(),
        page.records.len(),
        String::from_utf8_lossy(&got[..got.len().min(200)])
    );
    let text = m.ring_text(0, usize::MAX);
    kassert!(text.windows(15).any(|w| w == b"I klog ring.ear"), "klog's first records are buffered");
    kassert!(text.windows(17).any(|w| w == b"I kernel start ve"), "the kernel's start line (before DHCP) is buffered");
    Ok(())
}
ktest!("rb.mirror.early_buffer", Safe, early_buffer);

/// The emergency path with interrupts off on a fake card: unsent text and the line as raw frames, the line
/// alone in the screen stream while the ring is held, no second copy from klogd, the end datagram on stop.
fn emergency_fake_card() -> TestResult {
    let card = FakeCard::new();
    let m = test_mirror(&FAKE, 64 << 10)?;
    use klog::Sink;
    m.write_line(klog::Level::Info, "[    0.000001] I rb before.live");
    m.set_route(Some(fake_route(&card)));
    let mut b = [0u8; DGRAM_MAX];
    let (_, start, _) = m.begin(0xC0FFEE, 0, [10, 0, 2, 15], &mut b).ok_or("begin")?;
    kassert!(start == 0);
    m.set_live(true);
    m.write_line(klog::Level::Info, "[    0.000002] I rb waiting");
    let line = "[    0.000003] E rb panic.like ä";
    let rep = {
        let _irq = hal::cpu::IrqGuard::new();
        let r = m.emergency(line);
        m.settle();
        r
    };
    kassert!(rep.frames == 1 && !rep.dropped && !rep.screen, "{:?}", rep);
    let text = log_text(&card.datagrams([10, 0, 2, 15])?, 0xC0FFEE, 0)?;
    let mut want = Vec::new();
    for l in ["[    0.000001] I rb before.live", "[    0.000002] I rb waiting", line] {
        want.extend(encoded(l));
        want.push(b'\n');
    }
    kassert!(text == want, "{:?}", String::from_utf8_lossy(&text));
    // klogd's later copy of the same record is not sent again.
    m.write_line(klog::Level::Error, line);
    kassert!(m.ring_state().0 == want.len() as u64, "duplicate appended");
    // Ring held (the panic interrupted its holder): the line alone, screen stream.
    card.frames.lock().clear();
    let rep = {
        let _held = m.st.lock();
        m.emergency("[    0.000004] E rb while.held")
    };
    kassert!(rep.screen && rep.frames == 1, "{:?}", rep);
    let d = card.datagrams([10, 0, 2, 15])?;
    kassert!(d.len() == 1 && d[0][3] == b'S' && d[0][HEAD..] == *b"[    0.000004] E rb while.held\n", "{:?}", d);
    // A full card: dropped, counted, the text stays for the next try.
    card.frames.lock().clear();
    card.refuse.store(true, core::sync::atomic::Ordering::Release);
    let rep = m.emergency("[    0.000005] E rb card.full");
    kassert!(rep.dropped && rep.frames == 0, "{:?}", rep);
    card.refuse.store(false, core::sync::atomic::Ordering::Release);
    // Quiesce: the waiting line, then the end datagram with both stream ends.
    let ended = {
        let _irq = hal::cpu::IrqGuard::new();
        m.stop()
    };
    kassert!(ended.ended && !ended.timed_out && !m.is_live(), "{:?}", ended);
    let d = card.datagrams([10, 0, 2, 15])?;
    let last = d.last().ok_or("nothing sent on stop")?;
    let (written, ..) = m.ring_state();
    kassert!(last[3] == b'E' && last[HEAD..HEAD + 8] == 31u64.to_le_bytes() && last[HEAD + 8..HEAD + 16] == written.to_le_bytes(), "{:?}", last);
    let text = log_text(&d, 0xC0FFEE, 0)?;
    kassert!(text.ends_with(b"E rb card.full\n"), "{:?}", String::from_utf8_lossy(&text));
    kassert!(m.emergency("after stop").frames == 0, "nothing after quiesce");
    Ok(())
}
ktest!("rb.mirror.emergency_fake_card", Safe, emergency_fake_card);

/// The mirror's ring follows the machine's memory (RAM / 8192, 256 KiB .. 8 MiB); the kernel's mirror, when
/// CONFIG names one, has exactly that unless memory was short.
fn sized() -> TestResult {
    let total = mm_core::frames().map(|f| f.stats().total_bytes).ok_or("no frames")?;
    let want = super::buf::bytes_for_memory(total);
    kassert!((256 << 10..=8 << 20).contains(&want), "{} for {} MB", want, total >> 20);
    if let Some(m) = super::mirror() {
        let cap = m.ring_state().4;
        kassert!(cap == want || (cap < want && want.is_multiple_of(cap)), "kernel mirror ring {} for {} MB (want {})", cap, total >> 20, want);
    }
    Ok(())
}
ktest!("rb.mirror.sized", Safe, sized);

// ---------------------------------------------------------------------------------------------------------
// The kernel's mirror (CONFIG `mirror=`; the scenario's listener checks what arrives)

fn kernel_mirror_live(timeout_s: u64) -> Result<&'static Mirror, String> {
    let Some(m) = super::mirror() else { return Err(String::from("\u{1}skip")) };
    let deadline = time::now_ns() + timeout_s * 1_000_000_000;
    while !m.is_live() {
        if time::now_ns() > deadline {
            return Err(alloc::format!("mirror not live after {timeout_s} s"));
        }
        sched::sleep_ms(20);
    }
    Ok(m)
}

fn live_or_skip() -> Result<Option<&'static Mirror>, String> {
    match kernel_mirror_live(60) {
        Ok(m) => Ok(Some(m)),
        Err(e) if e == "\u{1}skip" => Ok(None),
        Err(e) => Err(e),
    }
}

fn wait_sent(m: &Mirror, timeout_s: u64) -> TestResult {
    let deadline = time::now_ns() + timeout_s * 1_000_000_000;
    while m.pending() > 0 || klog::pending() {
        if time::now_ns() > deadline {
            return Err(alloc::format!("{} bytes still waiting after {timeout_s} s", m.pending()));
        }
        sched::sleep_ms(10);
    }
    Ok(())
}

fn counter(name: &str) -> u64 {
    klog::metrics::snapshot().find(|(n, _)| *n == name).map_or(0, |c| c.1)
}

fn live() -> TestResult {
    let Some(m) = live_or_skip()? else { return ktest::skip("no mirror= in CONFIG") };
    klog::event!(Warn, "rb", "mirror.live_test", id = klog::Hex(m.id() as u64));
    wait_sent(m, 20)?;
    // Loss and send errors are the scenario's business (its listener shows them); on the Mini-PC a cable that
    // was out for a while may have caused both, which is no failure of the mirror.
    let (written, sent, oldest, lost, _) = m.ring_state();
    kassert!(sent == written, "written={} sent={} oldest={} lost={}", written, sent, oldest, lost);
    kassert!(counter("mirror.datagrams") > 0);
    kassert!(m.route().is_some(), "no route snapshot for the emergency path");
    Ok(())
}
ktest!("rb.mirror.live", Safe, live);

fn emergency_line() -> TestResult {
    let Some(m) = live_or_skip()? else { return ktest::skip("no mirror= in CONFIG") };
    wait_sent(m, 20)?;
    let before = counter("mirror.emergency_frames");
    let mark = alloc::format!("{:x}", time::now_ns());
    {
        // As the panic path: interrupts off, `try_event!`, then the flush step.
        let _irq = hal::cpu::IrqGuard::new();
        klog::try_event!(Error, "rb", "mirror.emergency_line", marker = mark.as_str());
        klog::flush_emergency();
    }
    kassert!(counter("mirror.emergency_frames") > before, "no frame went through poll_tx");
    wait_sent(m, 20)?;
    Ok(())
}
ktest!("rb.mirror.emergency", Safe, emergency_line);

/// The way back after a quiesce whose Reload did not happen (the cards stayed up here): the mirror's quiesce
/// entry sends the end, `resume` makes the thread start a new view with a new id, and lines flow again. The
/// scenario's listener sees "mirror ended", a second start and the marker line after it, with nothing twice.
fn restart() -> TestResult {
    let Some(m) = live_or_skip()? else { return ktest::skip("no mirror= in CONFIG") };
    wait_sent(m, 20)?;
    let old = m.id();
    let n = devmodel::quiesce_matching("mirror");
    kassert!(n == 1, "{} quiesce entries named mirror (registered once)", n);
    kassert!(!m.is_live(), "still live after its quiesce");
    kassert!(m.resume(), "not stopped after its quiesce");
    let deadline = time::now_ns() + 30_000_000_000;
    while !(m.is_live() && m.id() != old) {
        if time::now_ns() > deadline {
            return Err(String::from("no new start 30 s after resume"));
        }
        sched::sleep_ms(20);
    }
    klog::event!(Warn, "rb", "mirror.after_restart", id = klog::Hex(m.id() as u64), previous = klog::Hex(old as u64));
    wait_sent(m, 20)?;
    kassert!(m.route().is_some(), "no route snapshot after the restart");
    Ok(())
}
ktest!("rb.mirror.restart", Safe, restart);

/// A real panic after the boot went out (only with CONFIG `rb_mirror_panic=on`): the scenario looks for its
/// line on the listener, which only the panic path (`try_event!` + `poll_tx`) can send.
fn forced_panic() -> TestResult {
    if kexec::boot::config("rb_mirror_panic").as_deref() != Some("on") {
        return ktest::skip("CONFIG rb_mirror_panic=on not set");
    }
    let m = kernel_mirror_live(60)?;
    klog::event!(Warn, "rb", "mirror.panic_next");
    wait_sent(m, 20)?;
    panic!("rb.mirror forced panic marker={:x}", m.id());
}
ktest!("rb.mirror.forced_panic", QemuOnly, forced_panic);
