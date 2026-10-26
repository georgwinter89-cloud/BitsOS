//! Register layout of an xHCI controller (xHCI 1.2 §5) and the capability parsing.
//!
//! Registers are read as dwords (V1 `xhcipci.asm`: CAPLENGTH and HCIVERSION share one dword). Everything the
//! controller reports is checked against the size of BAR0 before the driver uses it, so a broken or lying
//! controller ends in an error, never in an access outside the mapped window.

use alloc::vec::Vec;
use usb_core::Speed;

// Capability registers (§5.3).
pub const CAPLENGTH: usize = 0x00;
pub const HCSPARAMS1: usize = 0x04;
pub const HCSPARAMS2: usize = 0x08;
pub const HCCPARAMS1: usize = 0x10;
pub const DBOFF: usize = 0x14;
pub const RTSOFF: usize = 0x18;

// Operational registers (§5.4), relative to the operational base.
pub const USBCMD: usize = 0x00;
pub const USBSTS: usize = 0x04;
pub const PAGESIZE: usize = 0x08;
pub const CRCR: usize = 0x18;
pub const DCBAAP: usize = 0x30;
pub const CONFIG: usize = 0x38;
pub const PORTSC_BASE: usize = 0x400;
pub const PORT_STRIDE: usize = 0x10;

pub mod cmd {
    pub const RS: u32 = 1 << 0;
    pub const HCRST: u32 = 1 << 1;
    pub const INTE: u32 = 1 << 2;
    pub const HSEE: u32 = 1 << 3;
}

pub mod sts {
    pub const HCH: u32 = 1 << 0;
    pub const HSE: u32 = 1 << 2;
    pub const EINT: u32 = 1 << 3;
    pub const PCD: u32 = 1 << 4;
    pub const CNR: u32 = 1 << 11;
    pub const HCE: u32 = 1 << 12;
}

pub mod crcr {
    pub const RCS: u32 = 1 << 0;
    pub const CS: u32 = 1 << 1;
    pub const CA: u32 = 1 << 2;
    pub const CRR: u32 = 1 << 3;
}

pub mod portsc {
    pub const CCS: u32 = 1 << 0;
    pub const PED: u32 = 1 << 1;
    pub const OCA: u32 = 1 << 3;
    pub const PR: u32 = 1 << 4;
    /// Port Link State (bits 8:5); 0 = U0.
    pub const PLS_MASK: u32 = 0xF << 5;
    pub const PLS_SHIFT: u32 = 5;
    /// Link states that only a warm reset leaves (xHCI §4.19.1.2, Linux `hub_port_warm_reset_required`).
    pub const PLS_INACTIVE: u32 = 6;
    pub const PLS_COMPLIANCE: u32 = 10;
    pub const PP: u32 = 1 << 9;
    /// Port Speed (bits 13:10), a Protocol Speed ID.
    pub const SPEED_SHIFT: u32 = 10;
    pub const CSC: u32 = 1 << 17;
    pub const PEC: u32 = 1 << 18;
    pub const WRC: u32 = 1 << 19;
    pub const OCC: u32 = 1 << 20;
    pub const PRC: u32 = 1 << 21;
    pub const PLC: u32 = 1 << 22;
    pub const CEC: u32 = 1 << 23;
    /// Every change bit (RW1C, V1 `XP_CHANGES`).
    pub const CHANGES: u32 = 0x7F << 17;
    pub const WPR: u32 = 1 << 31;
    /// Bits a PORTSC write must carry over unchanged: power, indicator, wake enables (V1 `XP_KEEP`). Everything
    /// else is either RW1C (would clear a change by accident) or PED (writing 1 disables the port).
    pub const KEEP: u32 = PP | 3 << 14 | 7 << 25;
}

// Runtime registers (§5.5): interrupter `i` at runtime base + 0x20 + 32 * i.
pub const IR0: usize = 0x20;
pub const IR_STRIDE: usize = 0x20;
pub const IMAN: usize = 0x00;
pub const IMOD: usize = 0x04;
pub const ERSTSZ: usize = 0x08;
pub const ERSTBA: usize = 0x10;
pub const ERDP: usize = 0x18;

pub mod iman {
    pub const IP: u32 = 1 << 0;
    pub const IE: u32 = 1 << 1;
}

/// ERDP bit 3: Event Handler Busy, RW1C; written with the new dequeue pointer.
pub const ERDP_EHB: u64 = 1 << 3;

/// IMOD interval in 250 ns units: 160 = 40 us (Linux's value, V1 `xhcimsi.asm`). Written at start so MSI-X
/// (PC-3) only has to set IMAN.IE and USBCMD.INTE.
pub const IMOD_DEFAULT: u32 = 160;

/// Contexts are 32 or 64 bytes (HCCPARAMS1.CSZ). Every context offset is a multiple of this size
/// (V1 `xhcictx.asm:1-6`); QEMU only has 32, the Mini-PC 64.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextSize {
    B32,
    B64,
}

impl ContextSize {
    pub fn from_hccparams1(v: u32) -> Self {
        if v & (1 << 2) != 0 { ContextSize::B64 } else { ContextSize::B32 }
    }
    pub fn bytes(self) -> usize {
        match self {
            ContextSize::B32 => 32,
            ContextSize::B64 => 64,
        }
    }
    /// Byte offset of context `index` in a device context (0 = slot, 1 = EP0, DCI n = n).
    pub fn device_offset(self, index: usize) -> usize {
        index * self.bytes()
    }
    /// Byte offset of context `index` in an input context (input control context first, then the device
    /// context's entries shifted by one).
    pub fn input_offset(self, index: usize) -> usize {
        (index + 1) * self.bytes()
    }
    /// Size of a device context: slot + 31 endpoint contexts.
    pub fn device_context_len(self) -> usize {
        32 * self.bytes()
    }
    /// Size of an input context: input control + slot + 31 endpoint contexts.
    pub fn input_context_len(self) -> usize {
        33 * self.bytes()
    }
}

/// What the capability registers say, checked against BAR0's size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caps {
    pub caplength: usize,
    pub version: u16,
    pub max_slots: u8,
    pub max_intrs: u16,
    pub max_ports: u8,
    /// Event Ring Segment Table Max: the table may hold 2^erst_max entries.
    pub erst_max: u8,
    pub scratchpads: u32,
    pub ac64: bool,
    pub ppc: bool,
    pub csz: ContextSize,
    /// Byte offset of the first extended capability (0 = none).
    pub xecp: usize,
    pub dboff: usize,
    pub rtsoff: usize,
}

impl Caps {
    /// Parses the capability registers through `rd` (offset -> dword) for a BAR of `bar_len` bytes.
    pub fn parse(rd: impl Fn(usize) -> u32, bar_len: usize) -> Result<Caps, &'static str> {
        let d0 = rd(CAPLENGTH);
        if d0 == u32::MAX {
            return Err("capability registers read all ones (device gone or not decoding)");
        }
        let caplength = (d0 & 0xFF) as usize;
        if caplength < 0x20 {
            return Err("CAPLENGTH below 0x20");
        }
        let p1 = rd(HCSPARAMS1);
        let p2 = rd(HCSPARAMS2);
        let c1 = rd(HCCPARAMS1);
        let c = Caps {
            caplength,
            version: (d0 >> 16) as u16,
            max_slots: p1 as u8,
            max_intrs: ((p1 >> 8) & 0x7FF) as u16,
            max_ports: (p1 >> 24) as u8,
            erst_max: ((p2 >> 4) & 0xF) as u8,
            // Max Scratchpad Buffers: bits 25:21 are the high five bits, 31:27 the low five (§5.3.4).
            scratchpads: ((p2 >> 21) & 0x1F) << 5 | (p2 >> 27),
            ac64: c1 & 1 != 0,
            ppc: c1 & (1 << 3) != 0,
            csz: ContextSize::from_hccparams1(c1),
            xecp: ((c1 >> 16) as usize) * 4,
            dboff: (rd(DBOFF) & !0x3) as usize,
            rtsoff: (rd(RTSOFF) & !0x1F) as usize,
        };
        if c.max_slots == 0 || c.max_ports == 0 || c.max_intrs == 0 {
            return Err("HCSPARAMS1 names no slots, ports or interrupters");
        }
        let within = |start: usize, len: usize| start.checked_add(len).is_some_and(|e| e <= bar_len);
        if !within(caplength, PORTSC_BASE + PORT_STRIDE * c.max_ports as usize) {
            return Err("operational registers and ports reach past BAR0");
        }
        if !within(c.rtsoff, IR0 + IR_STRIDE) {
            return Err("runtime registers (interrupter 0) reach past BAR0");
        }
        if !within(c.dboff, 4 * (c.max_slots as usize + 1)) {
            return Err("doorbell array reaches past BAR0");
        }
        if c.xecp != 0 && !within(c.xecp, 4) {
            return Err("extended capabilities start past BAR0");
        }
        Ok(c)
    }

    pub fn op(&self, reg: usize) -> usize {
        self.caplength + reg
    }
    pub fn portsc(&self, port: u8) -> usize {
        self.caplength + PORTSC_BASE + PORT_STRIDE * (port as usize - 1)
    }
    /// Interrupter `i`'s register `reg`. Only interrupter 0 is used (and checked against the BAR) today.
    pub fn ir(&self, i: usize, reg: usize) -> usize {
        self.rtsoff + IR0 + IR_STRIDE * i + reg
    }
    pub fn doorbell(&self, slot: u8) -> usize {
        self.dboff + 4 * slot as usize
    }
    /// Event ring segments to use: two when the controller allows (Linux `ERST_MAX_SEGS`), else one (QEMU).
    pub fn event_segments(&self, wanted: usize) -> usize {
        wanted.clamp(1, 1usize << self.erst_max.min(15))
    }
}

/// A supported-protocol capability (ID 2): which root ports speak which USB major revision, and the controller's
/// own Protocol Speed IDs if it names them (PSIC > 0; otherwise the defaults of xHCI §7.2.2.1.1 apply).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Protocol {
    pub major: u8,
    pub minor: u8,
    /// First port (1-based) and number of ports.
    pub first_port: u8,
    pub ports: u8,
    pub slot_type: u8,
    /// Protocol Speed ID dwords (PSIC of them; the field is 4 bits wide).
    pub psi: Vec<u32>,
}

impl Protocol {
    pub fn covers(&self, port: u8) -> bool {
        port >= self.first_port && (port as u16) < self.first_port as u16 + self.ports as u16
    }

    /// The speed a port of this protocol reports as PSIV `psiv`: from the controller's PSI table when it names
    /// the ID (by bit rate and lane protocol), else the default IDs; an unknown ID on a USB 3 port is taken as
    /// SuperSpeed (the major revision says that much).
    pub fn speed(&self, psiv: u8) -> Option<Speed> {
        let from_table = self.psi.iter().find(|&&d| (d & 0xF) as u8 == psiv).and_then(|&d| {
            let exp = (d >> 4) & 3;
            let lane_ssp = (d >> 14) & 3 == 1;
            let bps = ((d >> 16) as u64).checked_mul(1000u64.pow(exp))?;
            Some(match self.major {
                2 if bps <= 1_500_000 => Speed::Low,
                2 if bps <= 12_000_000 => Speed::Full,
                2 => Speed::High,
                _ if lane_ssp || bps > 5_000_000_000 => Speed::SuperPlus,
                _ => Speed::Super,
            })
        });
        let default = Speed::from_xhci_psiv(psiv).filter(|s| s.is_superspeed() == (self.major == 3));
        from_table.or(default).or((self.major == 3).then_some(Speed::Super))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtCaps {
    /// Byte offset of USBLEGSUP (ID 1), if present.
    pub legacy: Option<usize>,
    pub protocols: Vec<Protocol>,
    /// Number of capabilities seen (for the log).
    pub seen: usize,
    /// Supported-protocol capabilities refused by `Protocol::valid` (for the log).
    pub bad_protocols: usize,
}

pub const EXT_LEGACY: u8 = 1;
pub const EXT_PROTOCOL: u8 = 2;
/// Name string of a supported-protocol capability (dword 1): "USB " (xHCI 1.2 §7.2.1).
pub const PROTOCOL_NAME_USB: u32 = 0x2042_5355;

impl Protocol {
    /// US-2 picks the slot type and the USB 2/3 path from this table, so an entry is only taken when its name is
    /// "USB ", it is USB 2 or 3, and its ports lie within the controller's (1..=max_ports, at least one).
    fn valid(&self, name: u32, max_ports: u8) -> bool {
        name == PROTOCOL_NAME_USB && matches!(self.major, 2 | 3) && self.first_port >= 1 && self.ports >= 1
            && self.first_port as u16 + self.ports as u16 - 1 <= max_ports as u16
    }
}

/// Walks the extended capability list. Bounded by the BAR: every step moves at least one dword on, so a list
/// has at most `bar_len / 4` entries and a broken one ends there (a bound against hostile input, not a limit on
/// a count of things; R1, which is why V1's 64-entry cap is not kept).
pub fn walk_ext_caps(rd: impl Fn(usize) -> u32, xecp: usize, bar_len: usize, max_ports: u8) -> ExtCaps {
    let mut out = ExtCaps::default();
    let mut off = xecp;
    let mut budget = bar_len / 4;
    while off != 0 && off.checked_add(16).is_some_and(|e| e <= bar_len) && budget > 0 {
        budget -= 1;
        let d0 = rd(off);
        if d0 == u32::MAX {
            break;
        }
        out.seen += 1;
        match d0 as u8 {
            EXT_LEGACY if out.legacy.is_none() => out.legacy = Some(off),
            EXT_PROTOCOL => {
                let d2 = rd(off + 8);
                let psic = (d2 >> 28) as usize;
                // PSI dwords follow dword 3; only those inside the BAR are read.
                let psi = (0..psic).map(|i| off + 16 + 4 * i).take_while(|&a| a + 4 <= bar_len).map(&rd).collect();
                let p = Protocol {
                    major: (d0 >> 24) as u8,
                    minor: (d0 >> 16) as u8,
                    first_port: d2 as u8,
                    ports: (d2 >> 8) as u8,
                    slot_type: (rd(off + 12) & 0x1F) as u8,
                    psi,
                };
                if p.valid(rd(off + 4), max_ports) {
                    out.protocols.push(p);
                } else {
                    out.bad_protocols += 1;
                }
            }
            _ => {}
        }
        let next = ((d0 >> 8) & 0xFF) as usize * 4;
        if next == 0 {
            break;
        }
        off += next;
    }
    out
}

/// USBLEGSUP (§7.1.1) bits and the USBLEGCTLSTS clean-up mask Linux uses (`quirk_usb_handoff_xhci`).
pub mod legacy {
    pub const BIOS_OWNED: u32 = 1 << 16;
    pub const OS_OWNED: u32 = 1 << 24;
    /// USBLEGCTLSTS: keep only the reserved bits, SMI enables off (`XHCI_LEGACY_DISABLE_SMI`) ...
    pub const CTL_KEEP: u32 = 0x000E_1FEE;
    /// ... and the RW1C SMI events cleared (`XHCI_LEGACY_SMI_EVENTS`).
    pub const CTL_EVENTS: u32 = 0xE000_0000;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regs(p2: u32, c1: u32) -> impl Fn(usize) -> u32 {
        move |o| match o {
            CAPLENGTH => 0x0120_0040,
            HCSPARAMS1 => 7 << 24 | 8 << 8 | 64,
            HCSPARAMS2 => p2,
            HCCPARAMS1 => c1,
            DBOFF => 0x3000,
            RTSOFF => 0x2000,
            _ => 0,
        }
    }

    #[test]
    fn scratchpad_count_hi_lo() {
        // hi = 1 (bit 21), lo = 2 (bits 31:27): 1 << 5 | 2 = 34.
        let c = Caps::parse(regs(1 << 21 | 2 << 27, 0), 0x10000).unwrap();
        assert_eq!(c.scratchpads, 34);
        let c = Caps::parse(regs(2 << 27, 0), 0x10000).unwrap();
        assert_eq!(c.scratchpads, 2, "the Mini-PC's two pages");
        let c = Caps::parse(regs(0x1F << 21 | 0x1F << 27, 0), 0x10000).unwrap();
        assert_eq!(c.scratchpads, 1023);
    }

    #[test]
    fn fields() {
        let c = Caps::parse(regs(1 << 4, 1 | 1 << 2 | 1 << 3 | 0x200 << 16), 0x10000).unwrap();
        assert_eq!((c.caplength, c.version, c.max_slots, c.max_intrs, c.max_ports), (0x40, 0x120, 64, 8, 7));
        assert!(c.ac64 && c.ppc);
        assert_eq!(c.csz, ContextSize::B64);
        assert_eq!(c.xecp, 0x800);
        assert_eq!(c.erst_max, 1);
        assert_eq!(c.event_segments(2), 2);
        assert_eq!(c.event_segments(8), 2);
        assert_eq!(c.portsc(1), 0x440);
        assert_eq!(c.portsc(7), 0x4A0);
        assert_eq!(c.ir(0, ERDP), 0x2038);
        assert_eq!(c.doorbell(0), 0x3000);
        let q = Caps::parse(regs(0, 0), 0x10000).unwrap();
        assert_eq!(q.csz, ContextSize::B32);
        assert_eq!(q.event_segments(2), 1, "QEMU: ERST Max 0 = one segment");
    }

    #[test]
    fn refuses_what_does_not_fit() {
        assert!(Caps::parse(|_| u32::MAX, 0x10000).is_err());
        assert!(Caps::parse(regs(0, 0), 0x2000).is_err(), "runtime registers outside the BAR");
        let small_cap = |o| if o == CAPLENGTH { 0x0100_0010 } else { regs(0, 0)(o) };
        assert!(Caps::parse(small_cap, 0x10000).is_err());
        let no_slots = |o| if o == HCSPARAMS1 { 7 << 24 | 1 << 8 } else { regs(0, 0)(o) };
        assert!(Caps::parse(no_slots, 0x10000).is_err());
        let far_db = |o| if o == DBOFF { 0xFFFF_FFFC } else { regs(0, 0)(o) };
        assert!(Caps::parse(far_db, 0x10000).is_err());
    }

    #[test]
    fn context_offsets_are_multiples_of_the_size() {
        for (s, b) in [(ContextSize::B32, 32), (ContextSize::B64, 64)] {
            assert_eq!(s.bytes(), b);
            assert_eq!(s.device_offset(1), b, "EP0");
            assert_eq!(s.device_offset(31), 31 * b);
            assert_eq!(s.input_offset(0), b, "slot context after the input control context");
            assert_eq!(s.device_context_len(), 32 * b);
            assert_eq!(s.input_context_len(), 33 * b);
        }
    }

    #[test]
    fn ext_caps_walk() {
        // legacy at 0x800 -> protocol USB2 at 0x810 -> protocol USB3 at 0x830 (end).
        let rd = |o: usize| match o {
            0x800 => 0x0000_0401,
            0x810 => 0x0200_0802,
            0x814 | 0x834 => PROTOCOL_NAME_USB,
            0x818 => 5 << 8 | 1,
            0x81C => 3,
            0x830 => 0x0310_0002,
            0x838 => 2 << 8 | 6,
            0x83C => 4,
            _ => 0,
        };
        let e = walk_ext_caps(rd, 0x800, 0x10000, 7);
        assert_eq!(e.legacy, Some(0x800));
        assert_eq!(e.seen, 3);
        assert_eq!(e.protocols.len(), 2);
        assert_eq!(e.protocols[0], Protocol { major: 2, minor: 0, first_port: 1, ports: 5, slot_type: 3, psi: alloc::vec![] });
        assert_eq!(e.protocols[1], Protocol { major: 3, minor: 0x10, first_port: 6, ports: 2, slot_type: 4, psi: alloc::vec![] });
        assert!(e.protocols[1].covers(7) && !e.protocols[1].covers(8) && !e.protocols[0].covers(6));
        // Default IDs without a PSI table; an unknown one on USB 3 is SuperSpeed, on USB 2 unknown.
        let (p2, p3) = (&e.protocols[0], &e.protocols[1]);
        assert_eq!((p2.speed(1), p2.speed(2), p2.speed(3)), (Some(Speed::Full), Some(Speed::Low), Some(Speed::High)));
        assert_eq!((p3.speed(4), p3.speed(5), p3.speed(9)), (Some(Speed::Super), Some(Speed::SuperPlus), Some(Speed::Super)));
        assert_eq!((p2.speed(4), p2.speed(9)), (None, None), "a SuperSpeed ID on a USB 2 port is no speed");
    }

    #[test]
    fn protocol_speed_ids_from_the_psi_table() {
        // USB 3.2 capability with PSIC 3 (Linux xhci-mem.c layout): ID 4 = 5 Gb/s SS, ID 6 = 10 Gb/s SSP,
        // ID 7 = 20 Gb/s SSP; a USB 2 capability with its own IDs 1 = 12 Mb/s, 2 = 1.5 Mb/s, 3 = 480 Mb/s.
        let psi = |id: u32, exp: u32, lp: u32, m: u32| id | exp << 4 | lp << 14 | m << 16;
        let rd = |o: usize| match o {
            0x800 => 0x0320_0802,
            0x804 | 0x824 => PROTOCOL_NAME_USB,
            0x808 => 3 << 28 | 2 << 8 | 5,
            0x80C => 4,
            0x810 => psi(4, 3, 0, 5),
            0x814 => psi(6, 3, 1, 10),
            0x818 => psi(7, 3, 1, 20),
            0x820 => 0x0200_0002,
            0x828 => 3 << 28 | 4 << 8 | 1,
            0x82C => 3,
            0x830 => psi(1, 2, 0, 12),
            0x834 => psi(2, 1, 0, 1500),
            0x838 => psi(3, 2, 0, 480),
            _ => 0,
        };
        let e = walk_ext_caps(rd, 0x800, 0x10000, 6);
        assert_eq!(e.protocols.len(), 2);
        let (p3, p2) = (&e.protocols[0], &e.protocols[1]);
        assert_eq!(p3.psi.len(), 3);
        assert_eq!((p3.speed(4), p3.speed(6), p3.speed(7)), (Some(Speed::Super), Some(Speed::SuperPlus), Some(Speed::SuperPlus)));
        assert_eq!(p3.speed(5), Some(Speed::SuperPlus), "not in the table: default ID");
        assert_eq!((p2.speed(1), p2.speed(2), p2.speed(3)), (Some(Speed::Full), Some(Speed::Low), Some(Speed::High)));
        // A PSI table running past the BAR is cut there.
        assert_eq!(walk_ext_caps(rd, 0x800, 0x814, 6).protocols[0].psi.len(), 1);
    }

    #[test]
    fn ext_caps_loop_and_edge_are_bounded() {
        // A capability whose "next" points to itself forever would be next = 0 (end); a list that runs off the
        // BAR ends at the BAR; a huge xecp is refused.
        let rd = |o: usize| if o.is_multiple_of(0x100) { 0x0000_4002 } else { 0 };
        let e = walk_ext_caps(rd, 0x100, 0x1000, 8);
        assert!(e.seen <= 0x1000 / 4);
        assert_eq!(walk_ext_caps(|_| 0x0000_0101, 0xFFF8, 0x10000, 8).seen, 0, "no room for a capability");
        assert_eq!(walk_ext_caps(|_| u32::MAX, 0x100, 0x10000, 8).seen, 0);
    }

    #[test]
    fn ext_caps_refuse_bad_protocol_entries() {
        // One capability per 0x20 bytes from 0x800: (name, major, first_port, ports); the controller has 7 ports.
        let entries: [(u32, u8, u8, u8); 7] = [
            (PROTOCOL_NAME_USB, 2, 1, 4), // good
            (0x2020_2020, 2, 5, 1),       // wrong name
            (PROTOCOL_NAME_USB, 4, 5, 1), // no such USB major
            (PROTOCOL_NAME_USB, 3, 0, 2), // port 0
            (PROTOCOL_NAME_USB, 3, 6, 3), // runs past port 7
            (PROTOCOL_NAME_USB, 3, 5, 0), // no ports
            (PROTOCOL_NAME_USB, 3, 5, 3), // good: ports 5..=7
        ];
        let rd = |o: usize| {
            let i = (o - 0x800) / 0x20;
            let Some(&(name, major, first, n)) = entries.get(i) else { return 0 };
            let next = if i + 1 < entries.len() { 0x08 << 8 } else { 0 };
            match (o - 0x800) % 0x20 {
                0 => (major as u32) << 24 | next | EXT_PROTOCOL as u32,
                4 => name,
                8 => (n as u32) << 8 | first as u32,
                12 => 3,
                _ => 0,
            }
        };
        let e = walk_ext_caps(rd, 0x800, 0x10000, 7);
        assert_eq!(e.seen, 7);
        assert_eq!(e.bad_protocols, 5);
        assert_eq!(e.protocols.len(), 2);
        assert_eq!((e.protocols[0].major, e.protocols[0].first_port, e.protocols[0].ports), (2, 1, 4));
        assert_eq!((e.protocols[1].major, e.protocols[1].first_port, e.protocols[1].ports), (3, 5, 3));
        // Port 255 + 255 ports must not overflow the check.
        let big = |o: usize| match o {
            0x800 => 0x0300_0002,
            0x804 => PROTOCOL_NAME_USB,
            0x808 => 0xFFFF,
            _ => 0,
        };
        assert_eq!(walk_ext_caps(big, 0x800, 0x10000, 255).bad_protocols, 1);
    }
}
