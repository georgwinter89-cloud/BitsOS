//! `_CST` (processor idle states) without an AML interpreter (FD-1, SC-9): a byte scan for
//! `Name(_CST, Package(){count, Package(){ResourceTemplate(){Register(..)}, type, latency, power}, ..})`, the
//! static form firmware uses (the Mini-PC's AMI firmware has one per CPU in an SSDT, all alike). A `_CST` a
//! Method returns is not a `Name` and is not read. Every read stays inside the package and the table.
//!
//! The checks follow Linux's `acpi_processor_evaluate_cst` (linux-6.6/drivers/acpi/acpi_processor.c:828-990):
//! the whole `_CST` is refused when its first element does not equal the number of states that follow; one
//! state that is not a 4-element package of (buffer, integer, integer, integer) is skipped, the others stay.
//! What a state means (entry method, type, latency) is judged by the idle code (`sched::cstate`), not here:
//! this module only says what the table holds. No limit on the number of states (R1); the table bounds it.

use crate::bytes::u8_at;
use crate::s5::pkg_length;
use alloc::vec::Vec;

const NAME_OP: u8 = 0x08;
const ROOT_CHAR: u8 = 0x5C;
const PACKAGE_OP: u8 = 0x12;
const VAR_PACKAGE_OP: u8 = 0x13;
const BUFFER_OP: u8 = 0x11;
const STRING_OP: u8 = 0x0D;
/// Large resource "Generic Register" descriptor (ACPI 6.5 §6.4.3.7) and its fixed length.
const GENERIC_REGISTER: u8 = 0x82;
const GENERIC_REGISTER_LEN: u16 = 12;
const SPACE_IO: u8 = 1;
const SPACE_FFH: u8 = 0x7F;

/// How a C-state is entered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    /// Read one byte from this system I/O address (ACPI P_LVLx style; AMD Zen enters CC6 this way). The
    /// full 64-bit address as the table gives it: the idle code refuses anything past 0xFFFF.
    Io(u64),
    /// Functional fixed hardware (MWAIT on x86) with this address (the MWAIT hint).
    FixedHw(u64),
    /// Any other address space (memory, ...), with its space id.
    Other(u8),
}

/// One `_CST` entry, as the table states it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CState {
    /// ACPI C-state type (1 = C1, 2 = C2, 3 = C3; anything else is kept, saturated at 255, and refused later).
    pub kind: u8,
    pub entry: Entry,
    /// Worst-case exit latency in microseconds (saturated at `u32::MAX`).
    pub latency_us: u32,
}

/// Why one element of a `_CST` was skipped (Linux logs the same cases and skips them too).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    NotPackage,
    NotFourElements,
    NotBuffer,
    /// The buffer holds no Generic Register descriptor.
    NoRegister,
    NotInteger,
}

impl Skip {
    pub fn name(self) -> &'static str {
        match self {
            Skip::NotPackage => "not_package",
            Skip::NotFourElements => "not_4_elements",
            Skip::NotBuffer => "not_buffer",
            Skip::NoRegister => "no_register",
            Skip::NotInteger => "not_integer",
        }
    }
}

/// A parsed `_CST`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cst {
    pub states: Vec<CState>,
    /// Skipped elements: (1-based element number, why).
    pub skipped: Vec<(usize, Skip)>,
    /// The scan stopped at an element it could not even step over (an encoding it does not know); the states
    /// before it are kept. Fewer states only ever make the idle loop shallower.
    pub truncated: bool,
}

/// Searches the AML of one table (the bytes after its 36-byte header); the first `_CST` that yields at least
/// one state counts.
pub fn find(table: &[u8]) -> Option<Cst> {
    let aml = table.get(crate::sdt::HEADER_LEN..)?;
    let mut i = 0;
    while i + 4 <= aml.len() {
        if &aml[i..i + 4] == b"_CST"
            && let Some(s) = name_at(aml, i)
        {
            return Some(s);
        }
        i += 1;
    }
    None
}

fn name_at(aml: &[u8], i: usize) -> Option<Cst> {
    let before = aml.get(..i)?;
    if !matches!(before, [.., NAME_OP] | [.., NAME_OP, ROOT_CHAR]) {
        return None;
    }
    let (body, mut p, count) = package(aml, i + 4)?;
    // Element 0: the number of states that follow; Linux wants exactly count - 1 and at least one.
    let (n, next) = integer(body, p)?;
    p = next;
    if n == 0 || Some(n) != (count as u64).checked_sub(1) {
        return None;
    }
    let mut out = Cst::default();
    for k in 1..=n as usize {
        let Some(len) = element_len(body, p) else {
            out.truncated = true;
            break;
        };
        match state(&body[..p + len], p) {
            Ok(st) => out.states.push(st),
            Err(why) => out.skipped.push((k, why)),
        }
        p += len;
    }
    (!out.states.is_empty()).then_some(out)
}

/// A package at `p` inside `aml`: (the bytes up to its end, offset of its first element, element count).
fn package(aml: &[u8], p: usize) -> Option<(&[u8], usize, u8)> {
    if u8_at(aml, p)? != PACKAGE_OP {
        return None;
    }
    let (end, first) = sized(aml, p)?;
    if first >= end {
        return None;
    }
    Some((&aml[..end], first + 1, aml[first]))
}

/// An object with a PkgLength after its opcode at `p`: (its end, the offset after the PkgLength), only when
/// the end lies inside `aml`.
fn sized(aml: &[u8], p: usize) -> Option<(usize, usize)> {
    let (len, len_bytes) = pkg_length(aml, p + 1)?;
    let end = (p + 1).checked_add(len)?;
    if end > aml.len() || len < len_bytes {
        return None;
    }
    Some((end, p + 1 + len_bytes))
}

/// How many bytes the data object at `p` spans (`None`: unknown encoding or past the end).
fn element_len(aml: &[u8], p: usize) -> Option<usize> {
    match u8_at(aml, p)? {
        PACKAGE_OP | VAR_PACKAGE_OP | BUFFER_OP => sized(aml, p).map(|(end, _)| end - p),
        STRING_OP => aml.get(p + 1..)?.iter().position(|&b| b == 0).map(|z| z + 2),
        _ => integer(aml, p).map(|(_, next)| next - p),
    }
}

/// One state package: (Buffer with a Generic Register, type, latency, power). `aml` ends where it ends.
fn state(aml: &[u8], p: usize) -> Result<CState, Skip> {
    let (body, q, count) = package(aml, p).ok_or(Skip::NotPackage)?;
    if count != 4 {
        return Err(Skip::NotFourElements);
    }
    if u8_at(body, q) != Some(BUFFER_OP) {
        return Err(Skip::NotBuffer);
    }
    let (bend, data) = sized(body, q).ok_or(Skip::NotBuffer)?;
    let (_size, reg) = integer(&body[..bend], data).ok_or(Skip::NotBuffer)?;
    let entry = register(&body[..bend], reg).ok_or(Skip::NoRegister)?;
    let (kind, next) = integer(body, bend).ok_or(Skip::NotInteger)?;
    let (latency, next) = integer(body, next).ok_or(Skip::NotInteger)?;
    let (_power, _) = integer(body, next).ok_or(Skip::NotInteger)?;
    Ok(CState { kind: kind.min(u8::MAX as u64) as u8, entry, latency_us: latency.min(u32::MAX as u64) as u32 })
}

/// The Generic Register descriptor at `p`.
fn register(b: &[u8], p: usize) -> Option<Entry> {
    if u8_at(b, p)? != GENERIC_REGISTER || crate::bytes::u16_at(b, p + 1)? != GENERIC_REGISTER_LEN {
        return None;
    }
    let space = u8_at(b, p + 3)?;
    let addr = crate::bytes::u64_at(b, p + 7)?;
    Some(match space {
        SPACE_IO => Entry::Io(addr),
        SPACE_FFH => Entry::FixedHw(addr),
        s => Entry::Other(s),
    })
}

/// An AML integer constant at `p`: value and the offset after it.
fn integer(aml: &[u8], p: usize) -> Option<(u64, usize)> {
    let op = u8_at(aml, p)?;
    Some(match op {
        0x00 => (0, p + 1),
        0x01 => (1, p + 1),
        0xFF => (u64::MAX, p + 1),
        0x0A => (u8_at(aml, p + 1)? as u64, p + 2),
        0x0B => (crate::bytes::u16_at(aml, p + 1)? as u64, p + 3),
        0x0C => (crate::bytes::u32_at(aml, p + 1)? as u64, p + 5),
        0x0E => (crate::bytes::u64_at(aml, p + 1)?, p + 9),
        _ => return None,
    })
}

/// Builds `_CST` AML the way firmware writes it (host tests, and the idle ktests' fake table path).
pub mod build {
    use alloc::vec;
    use alloc::vec::Vec;

    /// One state: Generic Register (space, address), type, latency (us), power (mW).
    #[derive(Clone, Copy, Debug)]
    pub struct State {
        pub space: u8,
        pub addr: u64,
        pub kind: u64,
        pub latency_us: u64,
        pub power: u64,
    }

    impl State {
        pub const fn io(kind: u64, port: u64, latency_us: u64) -> State {
            State { space: super::SPACE_IO, addr: port, kind, latency_us, power: 0 }
        }
        pub const fn ffh(kind: u64, hint: u64, latency_us: u64) -> State {
            State { space: super::SPACE_FFH, addr: hint, kind, latency_us, power: 0 }
        }
    }

    /// The AML integer encoding of `v` (the shortest prefix, as iasl writes it).
    pub fn int(v: u64) -> Vec<u8> {
        match v {
            0 => vec![0x00],
            1 => vec![0x01],
            2..=0xFF => vec![0x0A, v as u8],
            0x100..=0xFFFF => [&[0x0B][..], &(v as u16).to_le_bytes()].concat(),
            0x1_0000..=0xFFFF_FFFF => [&[0x0C][..], &(v as u32).to_le_bytes()].concat(),
            _ => [&[0x0E][..], &v.to_le_bytes()].concat(),
        }
    }

    /// `op PkgLength body` with the PkgLength in the shortest form (one byte up to 63, else two).
    pub fn sized(op: u8, body: &[u8]) -> Vec<u8> {
        let mut v = if body.len() < 0x3F {
            vec![op, (body.len() + 1) as u8]
        } else {
            let len = body.len() + 2;
            vec![op, 0x40 | (len & 0x0F) as u8, (len >> 4) as u8]
        };
        v.extend_from_slice(body);
        v
    }

    /// `Package(n){elements}`.
    pub fn package(count: u8, elements: &[u8]) -> Vec<u8> {
        sized(super::PACKAGE_OP, &[&[count][..], elements].concat())
    }

    /// One state package as firmware writes it.
    pub fn state(s: &State) -> Vec<u8> {
        let mut reg = vec![0x82, 0x0C, 0x00, s.space, 0x08, 0x00, 0x01];
        reg.extend_from_slice(&s.addr.to_le_bytes());
        reg.extend_from_slice(&[0x79, 0x00]);
        let buffer = sized(super::BUFFER_OP, &[int(reg.len() as u64), reg].concat());
        package(4, &[buffer, int(s.kind), int(s.latency_us), int(s.power)].concat())
    }

    /// `Name(_CST, Package(count){first, elements..})` from ready element bytes (tests put broken ones in).
    pub fn name_raw(count: u8, first: u64, elements: &[Vec<u8>]) -> Vec<u8> {
        let mut v = vec![0x08, b'_', b'C', b'S', b'T'];
        v.extend(package(count, &[int(first), elements.concat()].concat()));
        v
    }

    /// `Name(_CST, ..)` of these states.
    pub fn name(states: &[State]) -> Vec<u8> {
        let el: Vec<Vec<u8>> = states.iter().map(state).collect();
        name_raw(states.len() as u8 + 1, states.len() as u64, &el)
    }

    /// The bytes of a table (zero header) around `aml`, for `super::find`.
    pub fn table(aml: &[u8]) -> Vec<u8> {
        let mut t = vec![0u8; crate::sdt::HEADER_LEN];
        t.extend_from_slice(aml);
        t
    }

    /// The Mini-PC's states (Ryzen 7 PRO 8845HS, AMI firmware): C1 FFH, C2 port 0x414 18 us, C3 port 0x415
    /// 350 us.
    pub const MINIPC: [State; 3] = [State::ffh(1, 0, 1), State::io(2, 0x414, 18), State::io(3, 0x415, 350)];
}

#[cfg(test)]
mod tests;
