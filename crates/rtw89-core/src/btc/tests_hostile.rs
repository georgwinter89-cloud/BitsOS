//! "Never panic on chip values, firmware reports or state": the whole coexistence life (init, RFK
//! notifications, radio on, band switch, scan, association, packets, the track work, radio off,
//! power off, radio on again) against rfk1's hostile chip (constants, a random stream, the chip
//! model with one access in `n` faulty); a failure injected at every bus op and at every H2C of a
//! good run (the error comes back with its register, nothing after it on the bus, later calls
//! still never panic); random firmware report bytes; random state and requests. The debug build
//! panics on an overflow, so it is caught here.

use alloc::vec;
use alloc::vec::Vec;

use super::tests::{FW_CODE, RFE1, c2h_msg};
use super::*;
use crate::bus::{BusError, BusResult, MemWidth, Width};
use crate::rfk1::fake::RfFake;
use crate::rfk1::tests_hostile::{BUDGET, Hostile, Src};
use crate::tables::BtcSlot;

/// H2Cs go nowhere; the `fail_at`-th one fails as a full CH12 would
#[derive(Default)]
struct Sink {
    n: u64,
    fail_at: Option<u64>,
}

impl<B: Bus + ?Sized> BtcHooks<B> for Sink {
    fn h2c(&mut self, _: &mut B, _: &H2c, _: &[u8]) -> Rtw89Result<()> {
        self.n += 1;
        if Some(self.n) == self.fail_at {
            return Err(Rtw89Error::RingFull {
                reg: R_AX_CH12_TXBD_IDX,
                wp: 3,
                rp: 4,
            });
        }
        Ok(())
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn u32(&mut self) -> u32 {
        (self.next() >> 16) as u32
    }
    fn below(&mut self, n: u32) -> u32 {
        self.u32() % n.max(1)
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

fn link(r: &mut Rng) -> WlStaLink {
    WlStaLink {
        port: r.below(8),
        avg_rssi: r.u32(),
        stats: state::Traffic {
            tx_tfc_lv: r.below(6),
            rx_tfc_lv: r.below(6),
            tx_throughput: r.u32(),
            rx_throughput: r.u32(),
            ..Default::default()
        },
        tx_hw_rate: r.u32(),
        rx_hw_rate: r.below(0x200),
    }
}

/// The coexistence's whole life; every step's error is collected, none may panic
fn life<B: Bus + ?Sized>(b: &mut B, h: &mut dyn BtcHooks<B>, r: &mut Rng) -> Vec<Rtw89Error> {
    let mut errs = Vec::new();
    let mut keep = |x: Rtw89Result<()>| {
        if let Err(e) = x {
            errs.push(e);
        }
    };
    let mut st = BtcState::new(FW_CODE).unwrap();
    power_on(&mut st);
    keep(init(b, &mut st, h, RFE1, BTC_MODE_NORMAL));
    for (typ, state) in [
        (BTC_WRFKT_DACK, BTC_WRFK_START),
        (BTC_WRFKT_DACK, BTC_WRFK_STOP),
        (BTC_WRFKT_RXDCK, BTC_WRFK_START),
        (BTC_WRFKT_RXDCK, BTC_WRFK_STOP),
    ] {
        keep(role::wl_rfk(b, &mut st, h, 0x10, typ, state).map(|_| ()));
    }
    keep(radio_on(b, &mut st, h));
    keep(switch_band(b, &mut st, h, Band::B2g));
    keep(scan(b, &mut st, h, true, Band::B5g));
    keep(scan(b, &mut st, h, false, Band::B5g));
    let req = RoleReq::default();
    keep(role::role(b, &mut st, h, RoleState::Start, &req));
    keep(role::connect(b, &mut st, h, true, &req));
    keep(role::connect(b, &mut st, h, false, &req));
    for p in [Packet::Eapol, Packet::EapolEnd, Packet::Dhcp] {
        keep(role::packet(b, &mut st, h, p));
    }
    keep(role::act1_work(b, &mut st, h));
    let rep = r.bytes(200);
    c2h(&mut st, &c2h_msg(0, rep));
    for _ in 0..3 {
        let l = link(r);
        keep(wlsta::wl_sta(b, &mut st, h, Some(&l)));
    }
    keep(bt_scoreboard(b).map(|_| ()));
    keep(role::role(b, &mut st, h, RoleState::Disconnect, &req));
    keep(role::role(b, &mut st, h, RoleState::Stop, &req));
    keep(radio_off(b, &mut st, h));
    keep(power_off(b, &mut st, h));
    keep(radio_on(b, &mut st, h));
    keep(switch_band(b, &mut st, h, Band::B5g));
    errs
}

#[test]
fn a_hostile_chip_never_panics() {
    let mut srcs = vec![
        Src::Const(0),
        Src::Const(u32::MAX),
        Src::Const(0xDEAD_BEEF),
        Src::Const(0xEAEA_EAEA),
        Src::Rand,
    ];
    srcs.extend([3, 17, 500].map(Src::Flaky));
    for (i, src) in srcs.into_iter().enumerate() {
        for seed in 1..6u64 {
            let mut b = Hostile::new(src, seed * 7919 + i as u64);
            let mut r = Rng(seed * 31 + 1);
            for e in life(&mut b, &mut Sink::default(), &mut r) {
                assert!(
                    matches!(e, Rtw89Error::Bus(_) | Rtw89Error::BtcNotPorted { .. }),
                    "{src:?}: {e:?}"
                );
            }
            assert!(b.ops < BUDGET, "no endless wait");
        }
    }
}

/// RfFake with the CMAC on that fails its `at`-th op (a poll times out, any other op is `Gone`)
/// and refuses every op after it
struct FailAt {
    good: RfFake,
    n: u64,
    at: u64,
}

impl FailAt {
    fn op(&mut self, poll: Option<(u32, Width, u32, u32)>) -> BusResult<()> {
        self.n += 1;
        if self.n < self.at {
            return Ok(());
        }
        if self.n == self.at
            && let Some((addr, width, mask, want)) = poll
        {
            return Err(BusError::PollTimeout {
                addr,
                width,
                mask,
                want,
                last: !want & mask,
            });
        }
        Err(BusError::Gone)
    }
}

impl Bus for FailAt {
    fn read(&mut self, w: Width, addr: u32) -> BusResult<u32> {
        self.op(None)?;
        self.good.read(w, addr)
    }
    fn write(&mut self, w: Width, addr: u32, val: u32) -> BusResult<()> {
        self.op(None)?;
        self.good.write(w, addr, val)
    }
    fn rmw(&mut self, w: Width, addr: u32, clear: u32, set: u32) -> BusResult<()> {
        self.op(None)?;
        self.good.rmw(w, addr, clear, set)
    }
    fn poll(&mut self, w: Width, addr: u32, mask: u32, want: u32, ms: u16) -> BusResult<()> {
        self.op(Some((addr, w, mask, want)))?;
        self.good.poll(w, addr, mask, want, ms)
    }
    fn delay_us(&mut self, us: u32) -> BusResult<()> {
        self.op(None)?;
        self.good.delay_us(us)
    }
    fn cfg_read(&mut self, off: u16) -> BusResult<u32> {
        self.op(None)?;
        self.good.cfg_read(off)
    }
    fn cfg_write(&mut self, off: u16, val: u32) -> BusResult<()> {
        self.op(None)?;
        self.good.cfg_write(off, val)
    }
    fn dma_alloc(&mut self, size: usize, align: usize) -> BusResult<u32> {
        self.good.dma_alloc(size, align)
    }
    fn dma_free(&mut self, addr: u32, size: usize) -> BusResult<()> {
        self.good.dma_free(addr, size)
    }
    fn mem_write(&mut self, w: MemWidth, addr: u32, val: u64) -> BusResult<()> {
        self.op(None)?;
        self.good.mem_write(w, addr, val)
    }
    fn mem_read32(&mut self, addr: u32) -> BusResult<u32> {
        self.op(None)?;
        self.good.mem_read32(addr)
    }
    fn mem_write_bytes(&mut self, addr: u32, data: &[u8]) -> BusResult<()> {
        self.good.mem_write_bytes(addr, data)
    }
    fn mem_read_bytes(&mut self, addr: u32, out: &mut [u8]) -> BusResult<()> {
        self.good.mem_read_bytes(addr, out)
    }
    fn now_ms(&mut self) -> u64 {
        0
    }
}

fn fail_at(at: u64) -> FailAt {
    FailAt {
        good: super::tests::chip(true),
        n: 0,
        at,
    }
}

#[test]
fn a_failure_at_every_op_comes_back_with_its_register() {
    let mut good = fail_at(u64::MAX);
    let errs = life(&mut good, &mut Sink::default(), &mut Rng(5));
    assert!(errs.is_empty(), "{errs:?}");
    let ops = good.n;
    assert!(ops > 500, "{ops} ops in a good life");
    for at in 1..=ops {
        let mut b = fail_at(at);
        let errs = life(&mut b, &mut Sink::default(), &mut Rng(5));
        assert!(!errs.is_empty(), "op {at}: the failure came back");
        assert!(
            b.good.trace.len() < at as usize,
            "op {at}: nothing ran after it"
        );
        match &errs[0] {
            Rtw89Error::Bus(BusError::PollTimeout { addr, .. }) => assert!(
                *addr == R_AX_LTE_CTRL + 3 || *addr == crate::phy::io::BB + R_SWSI_V1,
                "op {at}: {addr:#x}"
            ),
            Rtw89Error::Bus(BusError::Gone) => {}
            e => panic!("op {at}: {e:?}"),
        }
    }
}

#[test]
fn a_failing_h2c_comes_back() {
    let mut good = Sink::default();
    assert!(life(&mut fail_at(u64::MAX), &mut good, &mut Rng(5)).is_empty());
    assert!(good.n > 40, "{} H2Cs in a good life", good.n);
    for at in 1..=good.n {
        let mut h = Sink {
            n: 0,
            fail_at: Some(at),
        };
        let errs = life(&mut fail_at(u64::MAX), &mut h, &mut Rng(5));
        assert!(
            matches!(errs.first(), Some(Rtw89Error::RingFull { .. })),
            "H2C {at}: {errs:?}"
        );
        // an init cut short leaves BT "on" (Linux sets it before the first run): later runs refuse
        assert!(
            errs[1..]
                .iter()
                .all(|e| matches!(e, Rtw89Error::BtcNotPorted { .. })),
            "H2C {at}: {errs:?}"
        );
    }
}

#[test]
fn random_report_bytes_never_panic() {
    let mut r = Rng(0x5eed);
    let mut st = BtcState::new(FW_CODE).unwrap();
    for i in 0..3000 {
        let n = r.below(if i % 3 == 0 { 2000 } else { 260 }) as usize;
        let mut p = r.bytes(n);
        // now and then a well-formed header with a known type and a known length
        if i % 2 == 0 && p.len() > 3 {
            p[0] = r.below(14) as u8;
            let len = [12u16, 80, 84, 152, 208, 1384][r.below(6) as usize];
            p[1..3].copy_from_slice(&len.to_le_bytes());
        }
        let func = [0, 1, 2, 0x80, 0x82, r.below(256)][r.below(6) as usize];
        let cls = [0x12, 0x12, 0x12, r.below(64)][r.below(4) as usize];
        let c = C2h {
            cls,
            ..c2h_msg(func, p)
        };
        c2h(&mut st, &c);
    }
    assert!(
        st.events().len() <= 256 && st.bt_reports().len() <= 256,
        "capped"
    );
}

#[test]
fn random_state_and_requests_never_panic() {
    let mut r = Rng(0xbeef);
    for round in 0..400 {
        let mut b = super::tests::chip(round % 2 == 0);
        let mut st = BtcState::new(FW_CODE).unwrap();
        let mut h = Sink::default();
        if round % 3 != 0 {
            let _ = init(&mut b, &mut st, &mut h, RFE1, BTC_MODE_NORMAL);
        }
        // wild state
        st.ver.max_role_num = r.below(256) as u8;
        st.ctrl.trace_step = r.u32();
        st.wl.role.link_mode = r.below(9);
        st.wl.rfk_info.state = r.below(5);
        st.wl.status.lps = r.below(4);
        st.wl.status.traffic_dir = r.u32();
        st.bt0.scbd_c2h = if round % 5 == 0 { r.u32() } else { 0 };
        st.dm.wl_btg_rx = r.below(5);
        st.dm.wl_pre_agc = r.below(5);
        st.dm.wl_pre_agc_rb = r.below(5);
        st.dm.set_ant_path = r.u32();
        st.dm.tdd_bind.bt_sel = r.below(4);
        st.dm.tdd_bind.wl_link_mode = r.below(9);
        st.btg_pos = r.below(3);
        let n = r.below(40) as usize;
        st.dm.slot = (0..n)
            .map(|_| BtcSlot {
                dur: r.u32() as u16,
                cxtbl: r.u32(),
                cxtype: r.u32() as u16,
            })
            .collect();
        st.dm.slot_now = st.dm.slot.iter().rev().copied().collect();
        for rl in st.wl.role.rlink.iter_mut() {
            *rl = state::Rlink {
                connected: r.below(2),
                active: r.below(2) == 1,
                role: r.u32(),
                rf_band: r.u32(),
                ch: r.u32(),
                bw: r.u32(),
                mode: r.u32(),
                pid: r.u32(),
                ..Default::default()
            };
        }
        // wild requests
        let band = [Band::B2g, Band::B5g, Band::B6g][r.below(3) as usize];
        let req = RoleReq {
            port: r.below(300),
            mac_id: r.u32(),
            mode: r.u32(),
            band,
            ch: r.u32(),
            bw: r.u32(),
        };
        let states = [
            RoleState::Start,
            RoleState::Stop,
            RoleState::ConnStart,
            RoleState::ConnEnd,
            RoleState::Disconnect,
        ];
        let _ = role::role(&mut b, &mut st, &mut h, states[r.below(5) as usize], &req);
        let _ = role::wl_rfk(&mut b, &mut st, &mut h, r.u32(), r.u32(), r.below(6));
        let _ = role::preserve_bt_time(&mut b, &st, r.u32());
        let _ = scan(&mut b, &mut st, &mut h, r.below(2) == 1, band);
        let _ = switch_band(&mut b, &mut st, &mut h, band);
        let _ = role::act1_work(&mut b, &mut st, &mut h);
        let l = link(&mut r);
        let _ = wlsta::wl_sta(&mut b, &mut st, &mut h, Some(&l));
        let _ = h2c::h2c_cx_role(&mut b, &st, &mut h);
        let _ = h2c::fw_set_slots(&mut b, &mut st, &mut h);
        let _ = run::set_policy(&mut b, &mut st, &mut h, r.u32(), r.u32());
        let _ = run::set_wl_tx_power(&mut b, &mut st, r.u32(), r.below(4));
        let _ = run::set_wl_rx_gain(&mut b, &mut st, r.u32(), r.below(4));
        let _ = hw::btc_set_wl_txpwr_ctrl(&mut b, r.u32());
        let _ = hw::btc_set_wl_rx_gain(&mut b, &mut st, r.u32());
        let _ = hw::btc_set_wl_pri(&mut b, r.u32(), r.below(2) == 1);
        let _ = hw::btc_get_bt_rssi(r.u32() as i32);
        hw::btc_set_rfe(&mut st, r.u32(), r.u32());
        let rep = r.bytes(300);
        c2h(&mut st, &c2h_msg(0, rep));
        let _ = radio_off(&mut b, &mut st, &mut h);
        let _ = power_off(&mut b, &mut st, &mut h);
        let _ = radio_on(&mut b, &mut st, &mut h);
        let chip = ChipFacts {
            rfe: r.u32(),
            cv: r.u32(),
        };
        let _ = init(&mut b, &mut st, &mut h, chip, r.below(6));
    }
}

#[test]
fn the_refusals_name_the_value() {
    let mut b = super::tests::chip(true);
    let mut h = Sink::default();
    let mut st = BtcState::new(FW_CODE).unwrap();
    init(&mut b, &mut st, &mut h, RFE1, BTC_MODE_NORMAL).unwrap();
    // a BT side that says it is on is no longer the WLAN-only model
    st.bt0.enable_now = 1;
    assert_eq!(
        role::act1_work(&mut b, &mut st, &mut h).unwrap_err(),
        Rtw89Error::BtcNotPorted {
            what: "coex run with BT on",
            value: 0
        }
    );
    st.bt0.enable_now = 0;
    // a policy other than the TDMA-off ones
    let bt_off = crate::tables::btc::act::BT_OFF;
    assert_eq!(
        run::set_policy(&mut b, &mut st, &mut h, 0x0105, bt_off).unwrap_err(),
        Rtw89Error::BtcNotPorted {
            what: "coex policy",
            value: 0x0105
        }
    );
    // a forced WL TX power (debugfs in Linux)
    assert_eq!(
        run::set_wl_tx_power(&mut b, &mut st, 10, 1).unwrap_err(),
        Rtw89Error::BtcNotPorted {
            what: "forced WL TX power",
            value: 10
        }
    );
    // two roles on one band
    let mut req = RoleReq::default();
    role::connect(&mut b, &mut st, &mut h, false, &req).unwrap();
    req.port = 1;
    assert_eq!(
        role::connect(&mut b, &mut st, &mut h, false, &req).unwrap_err(),
        Rtw89Error::BtcNotPorted {
            what: "two WLAN roles",
            value: 2
        }
    );
    // a port past the role slots is ignored and logged
    req.port = 6;
    role::connect(&mut b, &mut st, &mut h, true, &req).unwrap();
    assert_eq!(st.events().last(), Some(&BtcEvent::RoleIgnored { port: 6 }));
    // RF off: the H2C is refused and logged, not sent
    st.wl.status.rf_off = 1;
    st.wl.status.rf_off_pre = 1;
    let n = h.n;
    h2c::fw_set_slots(&mut b, &mut st, &mut h).unwrap();
    assert_eq!(h.n, n);
    assert_eq!(
        st.events().last(),
        Some(&BtcEvent::H2cRefused {
            func: SET_SLOT_TABLE,
            len: 2 + 8 * 18
        })
    );
    // the events read as text for the log
    let text = alloc::format!("{}", st.events().last().unwrap());
    assert!(text.contains("refused"), "{text}");
}
