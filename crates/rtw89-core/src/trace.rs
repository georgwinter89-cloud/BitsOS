//! V1's register trace: the 16-byte op record (kit/wlkit.mjs `Ops.bytes`,
//! V1 `wlan/rtw89/fake.asm:1-20`, op codes `io.inc:8-20`) and the text lines of
//! `bringup-golden.txt` (bringup-steps.mjs `traceText`, without the index).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Op kinds as kit/wlkit.mjs numbers them, and the fake chip's own (fake.asm)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum OpKind {
    Wr = 1,
    Rd = 2,
    Rmw = 3,
    Poll = 4,
    Delay = 5,
    Mwr = 6,
    Mrd = 7,
    Mcpy = 8,
    Cfgr = 9,
    Cfgw = 10,
    Mpoll = 11,
    Mark = 12,
    Now = 13,
    /// bytes into the fake's DMA memory, `c` of them follow; no trace line
    BulkW = 20,
    /// `c` bytes of the fake's DMA memory come back; no trace line
    BulkR = 21,
    /// a `# ` line, `c` bytes of text follow
    Note = 22,
    /// driver state the fake chip looks at (`a` what, `c` value)
    State = 23,
    /// 4 bytes come back once the fake handled everything before
    Sync = 24,
    /// the JS driver's `await d.flush()`
    Flush = 25,
    /// the ops the fake still queues are dropped (JS stop after a failure)
    Drop = 26,
    /// what the fake holds at register `a`, 4 bytes back; no trace line, no flush
    Peek = 27,
}

impl OpKind {
    pub fn from_code(k: u8) -> Option<OpKind> {
        use OpKind::*;
        Some(match k {
            1 => Wr,
            2 => Rd,
            3 => Rmw,
            4 => Poll,
            5 => Delay,
            6 => Mwr,
            7 => Mrd,
            8 => Mcpy,
            9 => Cfgr,
            10 => Cfgw,
            11 => Mpoll,
            12 => Mark,
            13 => Now,
            20 => BulkW,
            21 => BulkR,
            22 => Note,
            23 => State,
            24 => Sync,
            25 => Flush,
            26 => Drop,
            27 => Peek,
            _ => return None,
        })
    }

    /// The word the trace text uses (bringup-steps.mjs KIND)
    pub fn name(self) -> &'static str {
        use OpKind::*;
        match self {
            Wr => "wr",
            Rd => "rd",
            Rmw => "rmw",
            Poll => "poll",
            Delay => "delay",
            Mwr => "mwr",
            Mrd => "mrd",
            Mcpy => "mcpy",
            Cfgr => "cfgr",
            Cfgw => "cfgw",
            Mpoll => "mpoll",
            Mark => "mark",
            Now => "now",
            BulkW => "bulkw",
            BulkR => "bulkr",
            Note => "note",
            State => "state",
            Sync => "sync",
            Flush => "flush",
            Drop => "drop",
            Peek => "peek",
        }
    }
}

/// One op: kind, size in bytes, ms (polls), then a, b, c as the record has them
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Op {
    pub kind: OpKind,
    pub size: u8,
    pub ms: u16,
    pub a: u32,
    pub b: u32,
    pub c: u32,
}

impl Op {
    pub const fn new(kind: OpKind, size: u8, ms: u16, a: u32, b: u32, c: u32) -> Op {
        Op {
            kind,
            size,
            ms,
            a,
            b,
            c,
        }
    }

    /// The 16 bytes on the wire: kind, size, ms (le16), a, b, c (le32)
    pub fn record(&self) -> [u8; 16] {
        let mut r = [0u8; 16];
        r[0] = self.kind as u8;
        r[1] = self.size;
        r[2..4].copy_from_slice(&self.ms.to_le_bytes());
        r[4..8].copy_from_slice(&self.a.to_le_bytes());
        r[8..12].copy_from_slice(&self.b.to_le_bytes());
        r[12..16].copy_from_slice(&self.c.to_le_bytes());
        r
    }

    /// A record back into an op; `None` for an unknown kind
    pub fn from_record(r: &[u8; 16]) -> Option<Op> {
        let d = |i: usize| u32::from_le_bytes([r[i], r[i + 1], r[i + 2], r[i + 3]]);
        Some(Op {
            kind: OpKind::from_code(r[0])?,
            size: r[1],
            ms: u16::from_le_bytes([r[2], r[3]]),
            a: d(4),
            b: d(8),
            c: d(12),
        })
    }

    /// Whether the op is a line of the register trace (the fake's own ops are not)
    pub fn traced(&self) -> bool {
        (self.kind as u8) <= OpKind::Now as u8
    }

    /// The line of bringup-golden.txt: `rd32 0x000003F0 0x00000000 0x00000000`
    pub fn line(&self) -> String {
        use OpKind::*;
        let size = match self.kind {
            Wr | Rd | Rmw | Poll | Mwr | Mrd => format!("{}", u32::from(self.size) * 8),
            _ => String::new(),
        };
        let ms = match self.kind {
            Poll | Mpoll => format!(" {} ms", self.ms),
            _ => String::new(),
        };
        format!(
            "{}{} 0x{:08X} 0x{:08X} 0x{:08X}{}",
            self.kind.name(),
            size,
            self.a,
            self.b,
            self.c,
            ms
        )
    }
}

/// An entry of a recorded trace: an op, or a `# ` note between ops
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceEntry {
    Op(Op),
    Note(String),
}

impl TraceEntry {
    pub fn line(&self) -> String {
        match self {
            TraceEntry::Op(op) => op.line(),
            TraceEntry::Note(t) => format!("# {t}"),
        }
    }
}

/// The trace as the lines of bringup-golden.txt
pub fn lines(trace: &[TraceEntry]) -> Vec<String> {
    trace.iter().map(TraceEntry::line).collect()
}

/// Where two traces part: the first line (0-based) that differs, with both sides
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    pub line: usize,
    pub ours: Option<String>,
    pub golden: Option<String>,
}

/// `ours` against the first `ours.len()` lines of `golden`; any line too few or different is a mismatch
pub fn compare_prefix<S: AsRef<str>, G: AsRef<str>>(
    ours: &[S],
    golden: &[G],
) -> Result<(), Mismatch> {
    for (i, o) in ours.iter().enumerate() {
        let g = golden.get(i).map(|g| g.as_ref());
        if g != Some(o.as_ref()) {
            return Err(Mismatch {
                line: i,
                ours: Some(o.as_ref().into()),
                golden: g.map(Into::into),
            });
        }
    }
    Ok(())
}

/// A line of bringup-golden.txt back into its op (`None` for a `# ` note or a line
/// that is no op); the inverse of [`Op::line`]
pub fn parse_line(line: &str) -> Option<Op> {
    use OpKind::*;
    let mut w = line.split(' ');
    let head = w.next()?;
    let cut = head
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or(head.len());
    let (name, bits) = head.split_at(cut);
    let kind = [
        Wr, Rd, Rmw, Poll, Delay, Mwr, Mrd, Mcpy, Cfgr, Cfgw, Mpoll, Mark, Now,
    ]
    .into_iter()
    .find(|k| k.name() == name)?;
    let size = if bits.is_empty() {
        if matches!(kind, Cfgr | Cfgw) { 4 } else { 0 }
    } else {
        u8::try_from(bits.parse::<u32>().ok()? / 8).ok()?
    };
    let mut num = || u32::from_str_radix(w.next()?.trim_start_matches("0x"), 16).ok();
    let (a, b, c) = (num()?, num()?, num()?);
    let ms = match w.next() {
        Some(ms) => ms.parse().ok()?,
        None => 0,
    };
    Some(Op::new(kind, size, ms, a, b, c))
}

/// Sends one op of a recorded trace through a bus again (tests: the steps of modules not
/// ported yet, taken from bringup-golden.txt as they are). A poll's or a read's answer is
/// not looked at; only the register ops, config space and traced DMA words replay.
pub fn replay<B: crate::bus::Bus + ?Sized>(bus: &mut B, op: &Op) -> crate::bus::BusResult<()> {
    use crate::bus::{BusError, MemWidth, Width};
    use OpKind::*;
    let w = match op.size {
        1 => Width::W8,
        2 => Width::W16,
        _ => Width::W32,
    };
    match op.kind {
        Wr => bus.write(w, op.a, op.c),
        Rd => bus.read(w, op.a).map(|_| ()),
        Rmw => bus.rmw(w, op.a, op.b, op.c),
        Poll => bus.poll(w, op.a, op.b, op.c, op.ms),
        Delay => bus.delay_us(op.a),
        Cfgr => bus.cfg_read(op.a as u16).map(|_| ()),
        Cfgw => bus.cfg_write(op.a as u16, op.c),
        Mwr => match op.size {
            1 => bus.mem_write(MemWidth::M8, op.a, op.c.into()),
            8 => bus.mem_write(
                MemWidth::M64,
                op.a,
                (u64::from(op.b) << 32) | u64::from(op.c),
            ),
            _ => bus.mem_write(MemWidth::M32, op.a, op.c.into()),
        },
        Mrd => bus.mem_read32(op.a).map(|_| ()),
        _ => Err(BusError::Refused {
            kind: "replay",
            addr: op.a,
            len: 0,
        }),
    }
}
