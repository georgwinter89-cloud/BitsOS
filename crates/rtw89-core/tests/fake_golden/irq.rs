//! WL-9: the MSI line of the driver on V1's fake chip, counted. The fake has no interrupt
//! registers, so a model sits between the driver and the fake: HIMR0/PCIE_HIMR00/PCIE_HIMR10
//! as written, PCIE_HISR00 latched from the RX queue's state (RXDMA when the chip filled a new
//! buffer, RDU as long as the ring is full) and cleared by writing ones, and
//! an MSI on every rise of HISR & HIMR (an unmask with a cause still pending sends the next one,
//! as on the chip). The thread is the kernel's service loop (card.rs): woken by MSI it masks and
//! takes the causes (`irq_ack`), turns, unmasks (`irq_rearm`), and runs again at once while a
//! new MSI came. On 24.10. the Mini-PC counted ~70 000 MSIs a second for 9 s at bring-up
//! (`rx_unavailable` 274,336): the RX ring was taken only while linked.

use std::collections::VecDeque;

use rtw89_core::bus::{BusResult, MemWidth, Width};
use rtw89_core::drv::{DevConfig, Runner};
use rtw89_core::pci::intr::{self, R_AX_HIMR0, R_AX_PCIE_HIMR00, R_AX_PCIE_HIMR10};
use rtw89_core::pci::intr::{R_AX_HISR0, R_AX_PCIE_HISR00, R_AX_PCIE_HISR10};
use rtw89_core::sta::StaEnv;
use rtw89_core::tables::regs::*;
use rtw89_core::tracebus::FakeState;
use wifi::mlme::JoinTiming;

use super::sta::{FAKE_PASS, Fail, GoldenEnv, RecBus, SSID};
use super::*;

/// The interrupt registers the model keeps (the fake never sees them)
#[derive(Debug, Default)]
struct IrqModel {
    himr0: u32,
    himr00: u32,
    himr10: u32,
    hisr00: u32,
    /// the host index of RXQ the driver wrote last, the chip's index at the last look
    host: u32,
    hw: u32,
    len: u32,
    line: bool,
    msis: u64,
}

struct IrqBus<'a, 'm> {
    inner: RecBus<'a>,
    m: &'m mut IrqModel,
}

impl IrqBus<'_, '_> {
    // the chip's view after any access: causes latched from the RX queue, an MSI on a rise
    fn refresh(&mut self) -> BusResult<()> {
        let hw = (self.inner.read(Width::W32, R_AX_RXQ_RXBD_IDX)? & TXBD_HW_IDX_MASK) >> 16;
        let m = &mut *self.m;
        let held = (hw + m.len - m.host) % m.len;
        if hw != m.hw {
            m.hisr00 |= B_AX_RXDMA_INT;
            m.hw = hw;
        }
        if held + 1 >= m.len {
            m.hisr00 |= B_AX_RDU_INT;
        }
        let line = m.hisr00 & m.himr00 != 0;
        if line && !m.line {
            m.msis += 1;
        }
        m.line = line;
        Ok(())
    }
}

impl Bus for IrqBus<'_, '_> {
    fn read(&mut self, w: Width, addr: u32) -> BusResult<u32> {
        match addr {
            R_AX_HIMR0 => Ok(self.m.himr0),
            R_AX_PCIE_HIMR00 => Ok(self.m.himr00),
            R_AX_PCIE_HIMR10 => Ok(self.m.himr10),
            R_AX_PCIE_HISR00 => {
                self.refresh()?;
                Ok(self.m.hisr00)
            }
            R_AX_HISR0 | R_AX_PCIE_HISR10 => Ok(0),
            _ => self.inner.read(w, addr),
        }
    }
    fn write(&mut self, w: Width, addr: u32, val: u32) -> BusResult<()> {
        match addr {
            R_AX_HIMR0 => self.m.himr0 = val,
            R_AX_PCIE_HIMR00 => self.m.himr00 = val,
            R_AX_PCIE_HIMR10 => self.m.himr10 = val,
            R_AX_PCIE_HISR00 => self.m.hisr00 &= !val,
            R_AX_HISR0 | R_AX_PCIE_HISR10 => {}
            _ => {
                if addr == R_AX_RXQ_RXBD_IDX {
                    self.m.host = val & TXBD_HOST_IDX_MASK;
                }
                return self.inner.write(w, addr, val);
            }
        }
        self.refresh()
    }
    fn rmw(&mut self, w: Width, addr: u32, clear: u32, set: u32) -> BusResult<()> {
        self.inner.rmw(w, addr, clear, set)
    }
    fn poll(&mut self, w: Width, addr: u32, mask: u32, want: u32, ms: u16) -> BusResult<()> {
        self.inner.poll(w, addr, mask, want, ms)
    }
    fn delay_us(&mut self, us: u32) -> BusResult<()> {
        self.inner.delay_us(us)
    }
    fn cfg_read(&mut self, off: u16) -> BusResult<u32> {
        self.inner.cfg_read(off)
    }
    fn cfg_write(&mut self, off: u16, val: u32) -> BusResult<()> {
        self.inner.cfg_write(off, val)
    }
    fn dma_alloc(&mut self, size: usize, align: usize) -> BusResult<u32> {
        self.inner.dma_alloc(size, align)
    }
    fn dma_free(&mut self, addr: u32, size: usize) -> BusResult<()> {
        self.inner.dma_free(addr, size)
    }
    fn mem_write(&mut self, w: MemWidth, addr: u32, val: u64) -> BusResult<()> {
        self.inner.mem_write(w, addr, val)
    }
    fn mem_read32(&mut self, addr: u32) -> BusResult<u32> {
        self.inner.mem_read32(addr)
    }
    fn mem_write_bytes(&mut self, addr: u32, data: &[u8]) -> BusResult<()> {
        self.inner.mem_write_bytes(addr, data)
    }
    fn mem_read_bytes(&mut self, addr: u32, out: &mut [u8]) -> BusResult<()> {
        self.inner.mem_read_bytes(addr, out)
    }
    fn now_ms(&mut self) -> u64 {
        self.inner.now_ms()
    }
    fn note(&mut self, text: &str) -> BusResult<()> {
        self.inner.note(text)
    }
    fn flush(&mut self) -> BusResult<()> {
        self.inner.flush()
    }
}

/// GoldenEnv's virtual clock and fake settings through the model
struct IrqEnv<'g>(&'g mut GoldenEnv);

impl<'a, 'm> StaEnv<IrqBus<'a, 'm>> for IrqEnv<'_> {
    fn now_ms(&mut self, _: &mut IrqBus<'a, 'm>) -> u64 {
        self.0.now
    }
    fn sleep_ms(&mut self, _: &mut IrqBus<'a, 'm>, ms: u32) {
        self.0.now += u64::from(ms);
    }
    fn random(&mut self, out: &mut [u8]) -> bool {
        for b in out {
            self.0.rnd = self.0.rnd.wrapping_mul(29).wrapping_add(7);
            *b = self.0.rnd;
        }
        true
    }
    fn channel_set(&mut self, bus: &mut IrqBus<'a, 'm>, _: Band, ch: u32) {
        bus.inner.inner.state(FakeState::Chan, ch).unwrap();
    }
    fn scanning(&mut self, bus: &mut IrqBus<'a, 'm>, on: bool) {
        bus.inner
            .inner
            .state(FakeState::Scan, u32::from(on))
            .unwrap();
    }
}

/// The most a woken thread runs again at once before the test calls it a storm
const STORM: u32 = 200;

struct Rig {
    fake: Fake,
    r: Runner,
    env: GoldenEnv,
    m: IrqModel,
    tx: VecDeque<Vec<u8>>,
    seen: u64,
    log: Vec<String>,
}

impl Rig {
    /// One wake of the service thread (card.rs service) and the runs a pending MSI makes at
    /// once; the number of runs
    fn wake(&mut self) -> u32 {
        let mut bus = IrqBus {
            inner: RecBus {
                inner: &mut self.fake.bus,
                writes: Vec::new(),
                fail: Fail::None,
            },
            m: &mut self.m,
        };
        let mut env = IrqEnv(&mut self.env);
        bus.refresh().unwrap();
        let mut runs = 0;
        loop {
            runs += 1;
            let by_irq = bus.m.msis != self.seen && self.r.irq.is_some();
            self.seen = bus.m.msis;
            if by_irq {
                let now = env.now_ms(&mut bus);
                self.r.irq_ack(&mut bus, now);
            }
            let out = self.r.turn(&mut bus, &mut env, &mut self.tx);
            if by_irq {
                self.r.irq_rearm(&mut bus);
            }
            bus.refresh().unwrap();
            assert_ne!(out.link, Some(false), "no link loss here");
            if bus.m.msis == self.seen || runs >= STORM {
                break;
            }
        }
        for e in self.r.log.drain(..) {
            self.log.push(format!("{} {}", e.name, e.text));
        }
        runs
    }
}

/// The rig on a fresh fake, the chip up; `irq`: MSI (else polled, as without MSI or net_irq=off)
fn rig(name: &str, irq: bool) -> Rig {
    let rings = RingConfig::js();
    let fw = Firmware::parse(firmware(), 1, false).expect("rtw89_fw_recognize");
    let mut r = Runner::new(
        fw,
        DevConfig {
            rings,
            keep_mac: None,
        },
    );
    r.irq = irq.then(intr::config_intr_mask);
    let mut g = Rig {
        fake: start(name, &["--sta"]),
        r,
        env: GoldenEnv {
            now: 0,
            rnd: 3,
            real: None,
        },
        m: IrqModel {
            len: rings.rxq,
            ..IrqModel::default()
        },
        tx: VecDeque::new(),
        seen: 0,
        log: Vec::new(),
    };
    // the bring-up: the masks come only with the chip up (rtw89_pci_ops_start); with HIMR at 0
    // until then the line cannot rise during the bring-up, which this checks stays so
    g.wake();
    assert!(g.r.dev.is_some(), "chip up: {:#?}", g.log);
    let want = if irq {
        intr::config_intr_mask().intrs[0]
    } else {
        0
    };
    assert_eq!(g.m.himr00, want);
    assert!(g.m.msis <= 1, "MSIs during the bring-up: {}", g.m.msis);
    g
}

fn network(pass: &[u8]) -> rtw89_core::sta::supervise::WlanCfg {
    rtw89_core::sta::supervise::WlanCfg {
        ssid: SSID.to_vec(),
        pmk: wifi::crypto::psk(pass, SSID).unwrap(),
        band: Band::B2g,
        channels: vec![1, 6, 11],
        dwell_ms: 20,
        timing: JoinTiming::default(),
    }
}

impl Rig {
    /// `n` beacons 10 ms apart, one wake each; none may storm
    fn beacons(&mut self, n: usize, what: &str) {
        for _ in 0..n {
            assert_eq!(self.fake.ask("ap beacon"), "ok");
            let runs = self.wake();
            assert!(runs < STORM, "MSI storm {what}: {} MSIs", self.m.msis);
            self.env.now += 10;
        }
    }
}

#[test]
fn driver_msi_stays_quiet_at_bring_up_and_while_idle() {
    if skip("driver_msi_stays_quiet_at_bring_up_and_while_idle") {
        return;
    }
    let mut g = rig("drv-irq", true);
    let at_up = g.m.msis;
    // idle without a network: a beacon every 10 ms for 3 s, three times the ring
    let frames = 3 * 100;
    g.beacons(frames, "while idle");
    let idle = g.m.msis - at_up;
    assert!(
        idle <= frames as u64 + 2,
        "{idle} MSIs for {frames} frames while idle"
    );
    assert_eq!(g.r.stats.rx_unavailable, 0, "the ring never ran full");
    assert!(
        g.r.stats.rx_idle >= frames as u64 - 1,
        "the idle frames were taken: {}",
        g.r.stats.rx_idle
    );
    // the network joined: the scan and the join take their own frames, the MSIs stay few
    g.r.set_network(Some(network(FAKE_PASS)), g.env.now);
    let before = g.m.msis;
    let mut linked = false;
    for _ in 0..400 {
        let runs = g.wake();
        assert!(runs < STORM, "MSI storm while joining: {} MSIs", g.m.msis);
        if g.r.link.is_some() {
            linked = true;
            break;
        }
        let next = g.r.next_wake(g.env.now);
        g.env.now = next.max(g.env.now + 1);
    }
    assert!(linked, "joined: {:#?}", g.log);
    let join = g.m.msis - before;
    assert!(join <= 20, "{join} MSIs for the join");
    assert!(
        !g.log.iter().any(|l| l.starts_with("irq.")),
        "no interrupt trouble: {:#?}",
        g.log
    );
}

/// The 24.10. storm lasted until the second join: the wait after a failed join is idle time too
#[test]
fn driver_msi_stays_quiet_after_a_failed_join() {
    if skip("driver_msi_stays_quiet_after_a_failed_join") {
        return;
    }
    let mut g = rig("drv-irq-fail", true);
    g.r.set_network(
        Some(network(b"not the passphrase of the fake AP")),
        g.env.now,
    );
    for _ in 0..400 {
        let runs = g.wake();
        assert!(runs < STORM, "MSI storm while joining: {} MSIs", g.m.msis);
        if g.r.stats.join_failures > 0 {
            break;
        }
        let next = g.r.next_wake(g.env.now);
        g.env.now = next.max(g.env.now + 1);
    }
    assert_eq!(g.r.stats.join_failures, 1, "the join failed: {:#?}", g.log);
    // the wait before the next join (2 s): beacons for 1.5 s, the ring taken all the same
    let (msis, idle, rdu) = (g.m.msis, g.r.stats.rx_idle, g.r.stats.rx_unavailable);
    let frames = 150;
    g.beacons(frames, "after the failed join");
    assert_eq!(g.r.stats.join_failures, 1, "no join yet");
    let n = g.m.msis - msis;
    assert!(
        n <= frames as u64 + 2,
        "{n} MSIs for {frames} frames after the failed join"
    );
    assert_eq!(g.r.stats.rx_unavailable, rdu, "the ring never ran full");
    assert!(
        g.r.stats.rx_idle - idle >= frames as u64 - 1,
        "frames taken: {}",
        g.r.stats.rx_idle - idle
    );
}

/// Polled (no MSI): the turn takes the ring while idle all the same
#[test]
fn driver_polled_takes_the_ring_while_idle() {
    if skip("driver_polled_takes_the_ring_while_idle") {
        return;
    }
    let mut g = rig("drv-irq-polled", false);
    let frames = 3 * 100;
    g.beacons(frames, "polled");
    assert!(
        g.r.stats.rx_idle >= frames as u64 - 1,
        "the idle frames were taken: {}",
        g.r.stats.rx_idle
    );
    assert_eq!(g.m.msis, 0, "no MSI without masks");
}
