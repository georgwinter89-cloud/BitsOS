//! SN-5: a register image of the Mini-PC's display engine (saved-5.json, 04.10.2026) that behaves as the
//! hardware: the endpoint registers sit behind the index (0xE118) and data (0xE11C) registers, a per-register keep
//! mask models read-only and self-clearing bits, and `DcnRegs`' full read-back compare. For the engine's tests here
//! and for the kernel glue's tests in `hda` (feature `fake`); not for the kernel.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::dpaudio::{self, EP_DATA, EP_INDEX, Loc, RegError, Regs};

pub const SAVED: &str = include_str!("../fixtures/dpaudio-saved-5.json");

/// The 13 gate lines' registers in their order (the 14th, 0x157AC, is in "D").
pub const GATE_REGS: [Loc; 13] = [
    Loc::Bar(0xE318),
    Loc::Bar(0x508),
    Loc::Bar(0x504),
    Loc::Bar(0x500),
    Loc::Bar(0x155C8),
    Loc::Bar(0x155C4),
    Loc::Bar(0x1552C),
    Loc::Bar(0x15720),
    Loc::Bar(0x15730),
    Loc::Bar(0x14004),
    Loc::Bar(0xE32C),
    Loc::Bar(0xE334),
    Loc::Ep(0x56),
];

#[derive(Clone, Debug, Default)]
pub struct Image {
    pub bar: BTreeMap<u32, u32>,
    pub ep: BTreeMap<u32, u32>,
    pub keep: BTreeMap<Loc, u32>,
    pub attached: bool,
    /// Logical writes: (where, value). An endpoint write is the data register's with the index it selected.
    pub log: Vec<(Loc, u32)>,
    /// Any access attempt to the index/data pair outside `with_pair`.
    pub unpaired: u32,
    pub depth: u32,
    pub accesses: u32,
    /// A dead BAR (device gone, link down): every read answers all ones.
    pub dead: bool,
    /// Reads still answered before the engine disappears (`NotAttached`); `None` = never.
    pub reads_left: Option<u32>,
}

impl Image {
    pub fn g(&self, l: Loc) -> u32 {
        match l {
            Loc::Bar(x) => self.bar.get(&x).copied().unwrap_or(0),
            Loc::Ep(x) => self.ep.get(&x).copied().unwrap_or(0),
        }
    }
    /// As the hardware takes a write: the bits its keep mask names stay.
    pub fn s(&mut self, l: Loc, v: u32) {
        let old = self.g(l);
        let keep = self.keep.get(&l).copied().unwrap_or(0);
        let new = old ^ ((old ^ v) & !keep);
        match l {
            Loc::Bar(x) => self.bar.insert(x, new),
            Loc::Ep(x) => self.ep.insert(x, new),
        };
    }
    pub fn index(&self) -> u32 {
        self.g(Loc::Bar(EP_INDEX)) & dpaudio::EP_INDEX_BITS
    }
    /// The registers both images hold, missing ones as 0 (the JS test's 512 KB RAM image).
    pub fn same(&self, o: &Image) -> Result<(), String> {
        let mut diff = Vec::new();
        for (name, a, b) in [("", &self.bar, &o.bar), ("E", &self.ep, &o.ep)] {
            let keys: alloc::collections::BTreeSet<u32> = a.keys().chain(b.keys()).copied().collect();
            for k in keys {
                let (x, y) = (a.get(&k).copied().unwrap_or(0), b.get(&k).copied().unwrap_or(0));
                if x != y {
                    diff.push(format!("{name}{k:X}: {x:X} -> {y:X}"));
                }
            }
        }
        if diff.is_empty() { Ok(()) } else { Err(diff.join(", ")) }
    }
    pub fn written(&self) -> usize {
        self.log.len()
    }
}

impl Regs for Image {
    fn read(&mut self, byte: u32) -> Result<u32, RegError> {
        self.accesses += 1;
        if !self.attached {
            return Err(RegError::NotAttached);
        }
        if let Some(n) = self.reads_left.as_mut() {
            if *n == 0 {
                return Err(RegError::NotAttached);
            }
            *n -= 1;
        }
        if self.dead {
            return Ok(!0);
        }
        if (byte == EP_INDEX || byte == EP_DATA) && self.depth == 0 {
            self.unpaired += 1;
        }
        Ok(if byte == EP_DATA { self.g(Loc::Ep(self.index())) } else { self.g(Loc::Bar(byte)) })
    }
    fn write(&mut self, byte: u32, v: u32) -> Result<(), RegError> {
        self.accesses += 1;
        if !self.attached {
            return Err(RegError::NotAttached);
        }
        if byte == EP_DATA {
            if self.depth == 0 {
                self.unpaired += 1;
            }
            let l = Loc::Ep(self.index());
            self.log.push((l, v));
            self.s(l, v);
        } else {
            if byte != EP_INDEX || self.depth == 0 {
                self.log.push((Loc::Bar(byte), v));
            }
            self.s(Loc::Bar(byte), v);
        }
        // The owner's full compare, as DcnRegs makes it (the engine must tolerate it for strobe bits).
        let back = if byte == EP_DATA { self.g(Loc::Ep(self.index())) } else { self.g(Loc::Bar(byte)) };
        if back != v { Err(RegError::ReadBack { read: back }) } else { Ok(()) }
    }
    fn with_pair<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T, RegError>) -> Result<T, RegError> {
        self.depth += 1;
        let r = f(self);
        self.depth -= 1;
        r
    }
}

pub fn hexval(s: &str) -> u32 {
    u32::from_str_radix(s, 16).expect("hex")
}

/// saved-5.json -> the image as the kit measured it, plus the neighbours dpaudio-image.mjs adds (never written).
pub fn firmware() -> Image {
    let mut img = Image { attached: true, ..Default::default() };
    let mut section = "";
    let mut gate = 0;
    for line in SAVED.lines() {
        let t = line.trim().trim_end_matches(',');
        if t.starts_with("\"GATE ok") {
            let v = t.rsplit_once('(').and_then(|(_, r)| r.strip_suffix(")\"")).expect("gate value");
            let l = GATE_REGS[gate];
            img.s(l, hexval(v));
            gate += 1;
        } else if let Some(name) = t.strip_suffix(": {") {
            section = match name {
                "\"D\"" => "D",
                "\"E\"" => "E",
                "\"X\"" => "X",
                _ => "",
            };
        } else if let Some((k, v)) = t.split_once(": ") {
            let (k, v) = (k.trim_matches('"'), v.trim_matches('"'));
            match section {
                "D" | "X" => img.s(Loc::Bar(hexval(k)), hexval(v)),
                "E" => img.s(Loc::Ep(hexval(k)), hexval(v)),
                _ => {}
            }
        }
    }
    assert_eq!(gate, 13, "13 gate lines");
    for (o, v) in [(0x510, 0xC000), (0x14204, 0x300), (0x5B0, 0), (0x5B4, 1), (0x154D8, 0x170), (0x17500, 0x8000)] {
        img.s(Loc::Bar(o), v);
    }
    img.s(Loc::Ep(0x21), 0x94);
    img.s(Loc::Ep(0x23), 0x7FFF_FFFF);
    img
}
