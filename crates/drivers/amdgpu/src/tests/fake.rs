//! The Mini-PC as V1's G-1 snapshot and saved-5.json show it, plus what neither read (V1 `dcn-fake.mjs`
//! `miniPc()`: pipes 1-3 blanked, OTG1-3 off), and the DMCUB's windows at the top of graphics memory.

use crate::attach::{BridgeWindows, Gop, PciFacts};
use crate::layout as l;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;

pub const BAR0: u64 = 0x7C_0000_0000;
pub const MB: u64 = 1 << 20;
pub const GPU: u64 = 0x80_0000_0000;
pub const GPU1: u64 = GPU + 32 * MB;
/// The GOP mode of G-1: 1024x768x4.
pub const BYTES: u64 = 1024 * 768 * 4;

pub type Image = BTreeMap<u32, u32>;

/// saved-5.json's direct gate registers, in the order of its "gate" lines (hdacodec `dpaudio.rs:76-85`).
const SAVED5_GATES: [u32; 10] = [0xE318, 0x508, 0x504, 0x500, 0x155C8, 0x155C4, 0x1552C, 0x15720, 0x15730, 0x14004];

pub fn saved5() -> Image {
    let j = mcp::Json::parse(include_str!("../../tests/data/saved-5.json")).unwrap();
    let mut m = Image::new();
    let hex = |s: &str| u32::from_str_radix(s, 16).unwrap();
    if let Some(mcp::Json::Obj(d)) = j.get("D") {
        for (k, v) in d {
            m.insert(hex(k), hex(v.as_str().unwrap()));
        }
    }
    let lines = j.get("gate").unwrap().as_array().unwrap();
    for (reg, line) in SAVED5_GATES.iter().zip(lines) {
        let s = line.as_str().unwrap();
        assert!(s.starts_with("GATE ok"), "{s}");
        let v = &s[s.rfind('(').unwrap() + 1..s.rfind(')').unwrap()];
        m.insert(*reg, hex(v));
    }
    m
}

pub fn mini_pc() -> Image {
    let mut m: Image = dcn314_regs::P1_SNAPSHOT.iter().copied().collect();
    for (k, v) in saved5() {
        m.entry(k).or_insert(v);
    }
    for d in 1..4 {
        m.insert(l::pg_status(d), 0x1000_0000);
    }
    for p in 1..l::PIPES {
        m.insert(l::hubp(p, l::HUBP_CNTL), 0xF101B | p << 4);
        m.insert(l::hubp(p, l::HUBP_FLIP), 0x410_0000);
        m.insert(l::hubp(p, l::HUBP_FLIP2), 0x40);
    }
    for k in 1..l::OTGS {
        m.insert(l::otg(k, l::OTG_CONTROL), 0x300);
        m.insert(l::dto_cntl(k), 0xC000);
    }
    // The DMCUB runs (DMCUB_CNTL 0x1900C6 in G-1) with a cache window at 0x81FFE00000 (HARDWARE-MINIPC.md:160).
    let w = &l::DMCUB_WINDOWS[0];
    m.insert(w.top, l::WINDOW_ON | 0xF_FFFF);
    m.insert(w.base.unwrap(), 0);
    m.insert(w.offset, 0xFFE0_0000);
    m.insert(w.offset_high, 0x81);
    m
}

/// GX-4b: the Mini-PC under V2's native GOP mode as `dcn_snapshot {}` read it on 22.10.2026 (279 registers,
/// plan/hw/dcn-ref-native-2026-10-22.json) over `mini_pc()` for what the snapshot does not read: the scaler in
/// manual 1:1 (AUTOCAL 0, SCL_MODE 1, ratios 1.0, one tap).
pub fn mini_pc_native() -> Image {
    let j = mcp::Json::parse(include_str!("../../tests/data/dcn-ref-native-2026-10-22.json")).unwrap();
    let mut m = mini_pc();
    let hex = |s: &str| u32::from_str_radix(s, 16).unwrap();
    let Some(mcp::Json::Obj(d)) = j.get("regs") else { panic!("no regs") };
    for (k, v) in d {
        m.insert(hex(k), hex(v.as_str().unwrap()));
    }
    m
}

pub fn reader(m: &Image) -> impl Fn(u32) -> Option<u32> + '_ {
    move |b| crate::snapshot::readable(b).then(|| m.get(&b).copied().unwrap_or(0))
}

pub fn pci() -> PciFacts {
    PciFacts {
        function: String::from("c7:00.0"),
        bus: 0xC7,
        vendor: 0x1002,
        class_code: 0x03,
        command: 0x0007,
        bar0: Some(BAR0),
        bar5: Some((0xDC50_0000, 0x8_0000)),
        bridges: Some(vec![BridgeWindows {
            mem: Some((0xDC00_0000, 0xDC5F_FFFF)),
            prefetch: Some((BAR0, BAR0 + 256 * MB - 1)),
        }]),
    }
}

pub fn gop() -> Gop {
    Gop { phys: BAR0, bytes: BYTES }
}
