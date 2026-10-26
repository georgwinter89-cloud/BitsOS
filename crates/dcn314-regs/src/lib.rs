//! DCN 3.1.4 register map of the Radeon 780M (Phoenix), as BAR5 byte offsets.
//!
//! Everything under `generated/` is produced by `host/gen/dcn314` from Linux's `dcn_3_1_4_offset.h` and
//! `dcn_3_1_4_sh_mask.h`; regenerate instead of editing. One module per register, named like the
//! header in lower case, e.g. `otg0_otg_master_update_lock::{REG, OTG_MASTER_UPDATE_LOCK}`. Indirect
//! registers (index/data pairs) are `ix_<name>::INDEX`. Fields are typed by their register, so a field
//! of one register cannot be applied to another register's value.
//!
//! This crate only names addresses; it never touches hardware.
#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

use core::marker::PhantomData;

#[rustfmt::skip]
mod generated;
pub use generated::*;
pub use generated::p1::P1_SNAPSHOT;

pub mod bar5;

/// A memory-mapped register: its BAR5 byte offset.
pub struct Reg<R> {
    name: &'static str,
    byte: u32,
    _r: PhantomData<fn() -> R>,
}

/// An indirect register: the index written to its block's index register.
pub struct Index<R> {
    name: &'static str,
    index: u32,
    _r: PhantomData<fn() -> R>,
}

/// A bit field of register `R`.
pub struct Field<R> {
    name: &'static str,
    shift: u8,
    mask: u32,
    _r: PhantomData<fn() -> R>,
}

/// A raw 32-bit value read from (or to be written to) register `R`.
pub struct Value<R> {
    raw: u32,
    _r: PhantomData<fn() -> R>,
}

// Manual impls: derives would demand `R: Clone` etc. of the marker types.
macro_rules! copy_eq {
    ($($t:ident),*) => {$(
        impl<R> Clone for $t<R> { fn clone(&self) -> Self { *self } }
        impl<R> Copy for $t<R> {}
        impl<R> core::fmt::Debug for $t<R> {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { self.fmt_inner(f) }
        }
    )*};
}
copy_eq!(Reg, Index, Field, Value);

impl<R> PartialEq for Value<R> {
    fn eq(&self, o: &Self) -> bool {
        self.raw == o.raw
    }
}
impl<R> Eq for Value<R> {}

impl<R> Reg<R> {
    pub const fn new(name: &'static str, byte: u32) -> Self {
        Self { name, byte, _r: PhantomData }
    }
    pub const fn name(self) -> &'static str {
        self.name
    }
    /// Byte offset in BAR5 (what a 32-bit MMIO access uses).
    pub const fn byte(self) -> u32 {
        self.byte
    }
    /// Absolute dword index (Linux's segment base + header offset).
    pub const fn dword(self) -> u32 {
        self.byte / 4
    }
    pub const fn value(self, raw: u32) -> Value<R> {
        Value::from_raw(raw)
    }
    fn fmt_inner(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}@{:#07x}", self.name, self.byte)
    }
}

impl<R> Index<R> {
    pub const fn new(name: &'static str, index: u32) -> Self {
        Self { name, index, _r: PhantomData }
    }
    pub const fn name(self) -> &'static str {
        self.name
    }
    pub const fn index(self) -> u32 {
        self.index
    }
    pub const fn value(self, raw: u32) -> Value<R> {
        Value::from_raw(raw)
    }
    fn fmt_inner(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}[{:#x}]", self.name, self.index)
    }
}

/// A value does not fit into the field it was meant for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldTooWide {
    pub field: &'static str,
    pub value: u32,
}

impl<R> Field<R> {
    /// Generated code only passes masks the generator checked (contiguous, starting at `shift`).
    pub const fn new(name: &'static str, shift: u32, mask: u32) -> Self {
        Self { name, shift: shift as u8, mask, _r: PhantomData }
    }
    pub const fn name(self) -> &'static str {
        self.name
    }
    pub const fn shift(self) -> u32 {
        self.shift as u32
    }
    pub const fn mask(self) -> u32 {
        self.mask
    }
    /// Largest value the field holds.
    pub const fn max(self) -> u32 {
        self.mask >> self.shift
    }
    pub const fn get(self, v: Value<R>) -> u32 {
        (v.raw & self.mask) >> self.shift
    }
    /// `v` with this field replaced by `x`; other bits unchanged. Too wide a value is an error.
    pub const fn set(self, v: Value<R>, x: u32) -> Result<Value<R>, FieldTooWide> {
        if x > self.max() {
            return Err(FieldTooWide { field: self.name, value: x });
        }
        Ok(Value::from_raw((v.raw & !self.mask) | (x << self.shift)))
    }
    pub const fn clear(self, v: Value<R>) -> Value<R> {
        Value::from_raw(v.raw & !self.mask)
    }
    pub const fn info(self) -> FieldInfo {
        FieldInfo { name: self.name, shift: self.shift, mask: self.mask }
    }
    fn fmt_inner(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}[{:#010x}]", self.name, self.mask)
    }
}

impl<R> Value<R> {
    pub const fn from_raw(raw: u32) -> Self {
        Self { raw, _r: PhantomData }
    }
    pub const fn raw(self) -> u32 {
        self.raw
    }
    pub const fn get(self, f: Field<R>) -> u32 {
        f.get(self)
    }
    pub const fn with(self, f: Field<R>, x: u32) -> Result<Self, FieldTooWide> {
        f.set(self, x)
    }
    fn fmt_inner(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:#010x}", self.raw)
    }
}

/// Untyped field description, for tables, decoding and logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldInfo {
    pub name: &'static str,
    pub shift: u8,
    pub mask: u32,
}

impl FieldInfo {
    pub const fn get(&self, raw: u32) -> u32 {
        (raw & self.mask) >> self.shift
    }
}

/// Untyped register description.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegInfo {
    pub name: &'static str,
    /// Index into `SEG_BASES` (the header's `_BASE_IDX`).
    pub seg: u8,
    /// Offset inside the segment, as in the header.
    pub dword: u32,
    pub byte: u32,
    pub fields: &'static [FieldInfo],
}

impl RegInfo {
    /// The non-zero fields of `raw`, in header order.
    pub fn decode(&self, raw: u32) -> impl Iterator<Item = (&'static str, u32)> + '_ {
        self.fields.iter().map(move |f| (f.name, f.get(raw))).filter(|&(_, x)| x != 0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexInfo {
    pub name: &'static str,
    /// Header address block; it names the index/data pair the index belongs to.
    pub block: &'static str,
    pub index: u32,
    pub fields: &'static [FieldInfo],
}

/// A header register this crate deliberately does not offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Excluded {
    pub name: &'static str,
    pub seg: Option<u8>,
    pub dword: u32,
    pub reason: &'static str,
}

/// Register by its header name (without the `reg` prefix).
pub fn lookup(name: &str) -> Option<&'static RegInfo> {
    REGISTERS.binary_search_by(|r| r.name.cmp(name)).ok().map(|i| &REGISTERS[i])
}

/// Every register at a BAR5 byte offset (several only for byte-wide VGA registers).
pub fn at_byte(byte: u32) -> impl Iterator<Item = &'static RegInfo> {
    let start = BY_BYTE.partition_point(|&(b, _)| b < byte);
    BY_BYTE[start..].iter().take_while(move |&&(b, _)| b == byte).filter_map(|&(_, i)| REGISTERS.get(i as usize))
}

pub fn lookup_indirect(name: &str) -> Option<&'static IndexInfo> {
    INDIRECT.binary_search_by(|r| r.name.cmp(name)).ok().map(|i| &INDIRECT[i])
}

pub fn excluded(name: &str) -> Option<&'static Excluded> {
    EXCLUDED.iter().find(|e| e.name == name)
}

/// BAR5 byte offset for a header (segment index, dword) pair; `None` for unknown segments,
/// segment 0 (aliases NBIO, X8) and anything outside the display window.
pub fn bar5_byte(seg: u32, dword: u32) -> Option<u32> {
    if seg == 0 {
        return None;
    }
    let base = *SEG_BASES.get(seg as usize)?;
    let b = base.checked_add(dword)?.checked_mul(4)?;
    (b < DISPLAY_WINDOW_END).then_some(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_set_is_checked() {
        let f = otg0_otg_master_update_lock::OTG_MASTER_UPDATE_LOCK;
        let v = otg0_otg_master_update_lock::REG.value(0x100);
        assert_eq!(f.set(v, 1).unwrap().raw(), 0x101);
        assert_eq!(f.set(v, 2), Err(FieldTooWide { field: "OTG_MASTER_UPDATE_LOCK", value: 2 }));
        assert_eq!(f.clear(f.set(v, 1).unwrap()).raw(), 0x100);
    }

    #[test]
    fn multi_bit_fields_round_trip_and_keep_other_bits() {
        use otg0_pixel_rate_cntl as p;
        let f = p::PIPE0_DTO_SRC_SEL; // bits 13:12
        assert_eq!((f.shift(), f.mask(), f.max()), (12, 0x3000, 3));
        let v = p::REG.value(0xFFFF_CFFF);
        let w = f.set(v, 3).unwrap();
        assert_eq!(w.raw(), 0xFFFF_FFFF);
        assert_eq!(w.get(f), 3);
        assert_eq!(f.set(v, 2).unwrap().raw(), 0xFFFF_EFFF);
        assert_eq!(f.set(v, 4), Err(FieldTooWide { field: "PIPE0_DTO_SRC_SEL", value: 4 }));
        assert_eq!(f.clear(w).raw(), 0xFFFF_CFFF);
        assert_eq!(w.get(p::OTG0_DIO_FIFO_ERROR), 3);

        use hubpreq0_dcsurf_primary_surface_address as a; // one field over all 32 bits
        let full = a::PRIMARY_SURFACE_ADDRESS;
        assert_eq!((full.shift(), full.max()), (0, u32::MAX));
        assert_eq!(full.set(a::REG.value(0), u32::MAX).unwrap().raw(), u32::MAX);
        use hubpreq0_dcsurf_surface_pitch as pitch; // 14 bits from 0, upper bits must survive
        let w = pitch::PITCH.set(pitch::REG.value(0xFFFF_0000), 0x3FFF).unwrap();
        assert_eq!(w.raw(), 0xFFFF_3FFF);
        assert!(pitch::PITCH.set(w, 0x4000).is_err());
    }

    #[test]
    fn every_field_obeys_set_get() {
        let samples = [0u32, u32::MAX, 0xA5A5_5A5A, 0x1234_5678, 0x8000_0001];
        let mut n = 0usize;
        for r in REGISTERS.iter().map(|r| r.fields).chain(INDIRECT.iter().map(|r| r.fields)) {
            for fi in r {
                let f = Field::<()>::new(fi.name, fi.shift as u32, fi.mask);
                let max = f.max();
                for &raw in &samples {
                    let v = Value::<()>::from_raw(raw);
                    assert_eq!(f.set(v, f.get(v)).unwrap(), v, "{}", fi.name);
                    for x in [0, 1 & max, max / 2, max] {
                        let w = f.set(v, x).unwrap();
                        assert_eq!(w.get(f), x, "{}", fi.name);
                        assert_eq!(w.raw() & !fi.mask, raw & !fi.mask, "{} touched other bits", fi.name);
                        assert_eq!(fi.get(w.raw()), x);
                    }
                    if let Some(over) = max.checked_add(1) {
                        assert!(f.set(v, over).is_err(), "{}", fi.name);
                    }
                    assert!(f.set(v, u32::MAX).is_err() || max == u32::MAX);
                    assert_eq!(f.clear(v).raw(), raw & !fi.mask);
                }
                n += 1;
            }
        }
        assert_eq!(n, 26_578, "every generated field was checked");
    }

    #[test]
    fn lookups_never_panic_on_odd_input() {
        assert!(lookup("").is_none());
        assert!(lookup("\u{0}\u{ffff}").is_none());
        assert_eq!(at_byte(u32::MAX).count(), 0);
        assert_eq!(bar5_byte(u32::MAX, u32::MAX), None);
        assert_eq!(bar5_byte(2, u32::MAX), None);
        assert_eq!(bar5_byte(0, 0), None);
        assert_eq!(bar5_byte(2, 0x1B8A), Some(0x14128));
    }

    #[test]
    fn tables_are_sorted_and_consistent() {
        assert!(REGISTERS.windows(2).all(|w| w[0].name < w[1].name));
        assert!(INDIRECT.windows(2).all(|w| w[0].name < w[1].name));
        assert!(BY_BYTE.windows(2).all(|w| w[0].0 <= w[1].0));
        assert_eq!(BY_BYTE.len(), REGISTERS.len());
        for r in REGISTERS {
            assert_eq!(Some(r.byte), bar5_byte(r.seg as u32, r.dword), "{}", r.name);
            assert!(at_byte(r.byte).any(|x| x.name == r.name));
            for f in r.fields {
                let m = f.mask >> f.shift;
                assert!(f.shift < 32 && m & 1 == 1 && m & m.wrapping_add(1) == 0 && m << f.shift == f.mask, "{}.{}", r.name, f.name);
            }
        }
    }
}
