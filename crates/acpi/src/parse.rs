//! From the RSDP to the parsed tables: the XSDT (RSDT as fallback), every listed table's header, the
//! tables BitsOS uses copied and checked, the DSDT through the FADT, `\_S5` in the DSDT or an SSDT.
//! Pure over `PhysMem`, so host tests drive it with hostile memory; problems become `Note`s, never panics.

use crate::bytes::u32_at;
use crate::mem::{self, Loaded, Metered, PhysMem, TableError, PARSE_BUDGET};
use crate::rsdp::{self, Rsdp, RsdpError};
use crate::sdt::{self, Header, Signature, HEADER_LEN};
use crate::{fadt, hpet, ivrs, madt, mcfg, s5, srat, Acpi, Note, Notes, RootKind, TableInfo};
use alloc::vec::Vec;

/// Tables copied and parsed (the first good one of each signature counts, as in V1 and Linux).
const USED: [Signature; 6] = [sdt::FADT, sdt::MADT, sdt::HPET, sdt::MCFG, sdt::IVRS, sdt::SRAT];

/// Reads every table with the default total budget (`mem::PARSE_BUDGET`).
pub fn parse(mem: &mut dyn PhysMem, rsdp_phys: u64) -> Acpi {
    parse_with_budget(mem, rsdp_phys, PARSE_BUDGET)
}

/// Reads at most `budget` bytes in all; what the budget cuts off is noted once (`budget_spent`).
pub fn parse_with_budget(mem: &mut dyn PhysMem, rsdp_phys: u64, budget: u64) -> Acpi {
    let mut m = Metered::new(mem, budget);
    let mem = &mut m;
    let mut a = Acpi::default();
    let r = match rsdp::read(mem, rsdp_phys) {
        Ok(r) => r,
        Err(e) => {
            let what = match e {
                RsdpError::Unreadable(_) => "unreadable",
                RsdpError::BadSignature => "bad_signature",
            };
            a.notes.push(Note::table(*b"RSDP", rsdp_phys, what, 0));
            return a;
        }
    };
    a.rsdp = Some(r);
    let Some((kind, entries)) = root(mem, &r, &mut a.notes) else {
        return a;
    };
    a.root = Some(kind);
    let mut used = [false; USED.len()];
    for (i, &phys) in entries.iter().enumerate() {
        if mem.spent() {
            a.notes.push(Note::table(kind.signature(), 0, "budget_spent", (entries.len() - i) as u64));
            break;
        }
        if phys == 0 {
            a.notes.push(Note::table(kind.signature(), 0, "null_entry", i as u64));
            continue;
        }
        listed(mem, phys, &mut a, &mut used);
    }
    dsdt_and_s5(mem, &mut a);
    a
}

/// The root table and its entry addresses: XSDT when the RSDP has a good one, else the RSDT.
fn root(mem: &mut dyn PhysMem, r: &Rsdp, notes: &mut Notes) -> Option<(RootKind, Vec<u64>)> {
    if !r.checksum_ok && !r.ext_checksum_ok {
        notes.push(Note::table(*b"RSDP", r.phys, "bad_checksum", 0));
        return None;
    }
    if r.revision >= 2 {
        if !r.ext_checksum_ok {
            notes.push(Note::table(*b"RSDP", r.phys, "bad_ext_checksum", 0));
        } else if r.xsdt == 0 {
            notes.push(Note::table(*b"RSDP", r.phys, "no_xsdt", 0));
        } else {
            match root_table(mem, r.xsdt, RootKind::Xsdt, notes) {
                Some(e) => return Some((RootKind::Xsdt, e)),
                None => notes.push(Note::table(sdt::XSDT, r.xsdt, "fallback_rsdt", r.rsdt as u64)),
            }
        }
    }
    if !r.checksum_ok || r.rsdt == 0 {
        notes.push(Note::table(sdt::RSDT, r.rsdt as u64, "no_root", 0));
        return None;
    }
    root_table(mem, r.rsdt as u64, RootKind::Rsdt, notes).map(|e| (RootKind::Rsdt, e))
}

fn root_table(mem: &mut dyn PhysMem, phys: u64, kind: RootKind, notes: &mut Notes) -> Option<Vec<u64>> {
    let sig = kind.signature();
    let t = match mem::load(mem, phys, Some(sig)) {
        Ok(t) => t,
        Err(e) => {
            notes.push(Note::table(sig, phys, e.name(), detail(&e)));
            return None;
        }
    };
    if !t.checksum_ok {
        notes.push(Note::table(sig, phys, "bad_checksum", 0));
        return None;
    }
    let w = kind.entry_size();
    let body = &t.bytes[HEADER_LEN..];
    if !body.len().is_multiple_of(w) {
        notes.push(Note::table(sig, phys, "odd_length", t.bytes.len() as u64));
    }
    let entries = body
        .chunks_exact(w)
        .map(|c| if w == 8 { crate::bytes::u64_at(c, 0).unwrap_or(0) } else { u32_at(c, 0).unwrap_or(0) as u64 })
        .collect();
    Some(entries)
}

/// One table the root lists: its header recorded; the used kinds copied, checked and parsed.
fn listed(mem: &mut dyn PhysMem, phys: u64, a: &mut Acpi, used: &mut [bool; USED.len()]) {
    let h = match mem::header(mem, phys) {
        Ok(h) => h,
        Err(e) => {
            a.notes.push(Note::table(*b"????", phys, e.name(), detail(&e)));
            return;
        }
    };
    let mut info = TableInfo { phys, header: h, checksum_ok: None, used: false };
    // A flag per used kind, not a search of `a.tables`: a hostile root lists millions of entries.
    let kind = USED.iter().position(|s| *s == h.signature);
    if let Some(k) = kind
        && !used[k]
    {
        match mem::load(mem, phys, Some(h.signature)) {
            Ok(t) if t.checksum_ok => {
                info.checksum_ok = Some(true);
                info.used = use_table(&t, a);
                used[k] = info.used;
                if info.used {
                    avoid_registers(mem, &h.signature, a);
                }
            }
            Ok(_) => {
                info.checksum_ok = Some(false);
                a.notes.push(Note::table(h.signature, phys, "bad_checksum", 0));
            }
            Err(e) => a.notes.push(Note::table(h.signature, phys, e.name(), detail(&e))),
        }
    } else if kind.is_some() {
        a.notes.push(Note::table(h.signature, phys, "duplicate", 0));
    }
    a.tables.push(info);
}

/// Register blocks a just-parsed table names, kept out of later table reads (a table pointer into a page of
/// registers must not trigger reads with side effects). Whole pages, as the reader maps whole pages.
fn avoid_registers(mem: &mut dyn PhysMem, sig: &Signature, a: &Acpi) {
    let mut page = |addr: u64, len: u64| {
        if addr != 0 {
            mem.avoid(addr & !0xFFF, addr.saturating_add(len).saturating_add(0xFFF) & !0xFFF);
        }
    };
    match sig {
        b"FACP" => {
            if let Some(f) = &a.fadt {
                let regs = [f.pm1a_evt, f.pm1b_evt, f.pm1a_cnt, f.pm1b_cnt, f.pm_tmr, f.gpe0, f.gpe1, f.sleep_control, f.sleep_status];
                for g in regs.iter().flatten().chain(f.reset.as_ref().map(|r| &r.reg)) {
                    if g.space == crate::sdt::Space::Memory {
                        page(g.address, g.access_bytes().max(1) as u64);
                    }
                }
            }
        }
        b"APIC" => {
            if let Some(m) = &a.madt {
                page(m.lapic_address, 0x1000);
                for io in &m.ioapics {
                    page(io.address as u64, 0x1000);
                }
            }
        }
        b"HPET" => {
            if let Some(b) = a.hpet.and_then(|h| h.mmio_base()) {
                page(b, 0x400);
            }
        }
        b"MCFG" => {
            for e in &a.mcfg {
                let (s, e) = e.window();
                mem.avoid(s, e);
            }
        }
        b"IVRS" => {
            if let Some(r) = &a.ivrs {
                // The IOMMU's MMIO block: 16 KiB, 512 KiB with extended features (Linux MMIO_REG_END_OFFSET).
                for u in &r.iommus {
                    page(u.base, 0x8_0000);
                }
            }
        }
        _ => {}
    }
}

/// Parses a checked table into `a`; false if the parser refused it.
fn use_table(t: &Loaded, a: &mut Acpi) -> bool {
    let (b, p, n) = (&t.bytes[..], t.phys, &mut a.notes);
    match &t.header.signature {
        b"FACP" => {
            a.fadt = fadt::parse(b, p, n);
            a.fadt.is_some()
        }
        b"APIC" => {
            a.madt = madt::parse(b, p, n);
            a.madt.is_some()
        }
        b"HPET" => {
            a.hpet = hpet::parse(b, p, n);
            a.hpet.is_some()
        }
        b"MCFG" => {
            a.mcfg = mcfg::parse(b, p, n);
            true
        }
        b"IVRS" => {
            a.ivrs = ivrs::parse(b, p, n);
            a.ivrs.is_some()
        }
        b"SRAT" => {
            a.srat = srat::parse(b, p, n);
            a.srat.is_some()
        }
        _ => false,
    }
}

/// The DSDT the FADT names (used even with a bad checksum, as ACPICA does, but noted), then `\_S5` in it
/// or in the first SSDT with a good checksum that has one (V1 `s5_find`).
fn dsdt_and_s5(mem: &mut dyn PhysMem, a: &mut Acpi) {
    let Some(dsdt) = a.fadt.as_ref().map(|f| f.dsdt).filter(|&d| d != 0) else {
        return;
    };
    match mem::load(mem, dsdt, Some(sdt::DSDT)) {
        Ok(t) => {
            if !t.checksum_ok {
                a.notes.push(Note::table(sdt::DSDT, dsdt, "bad_checksum", 0));
            }
            a.dsdt = Some(TableInfo { phys: dsdt, header: t.header, checksum_ok: Some(t.checksum_ok), used: true });
            if let Some(s) = s5::find(&t.bytes) {
                a.s5 = Some((s, sdt::DSDT, dsdt));
                return;
            }
        }
        Err(e) => a.notes.push(Note::table(sdt::DSDT, dsdt, e.name(), detail(&e))),
    }
    // Only SSDTs whose listed header was sane; `mem::load` re-checks and refuses what the budget cannot take
    // before allocating anything.
    let sane = |t: &&TableInfo| t.header.signature == sdt::SSDT && (HEADER_LEN as u32..=mem::TABLE_MAX).contains(&t.header.length);
    let ssdts: Vec<u64> = a.tables.iter().filter(sane).map(|t| t.phys).collect();
    for phys in ssdts {
        if mem.budget_left() < HEADER_LEN as u64 {
            a.notes.push(Note::table(sdt::SSDT, phys, "budget_spent", 0));
            return;
        }
        match mem::load(mem, phys, Some(sdt::SSDT)) {
            Ok(t) if t.checksum_ok => {
                if let Some(s) = s5::find(&t.bytes) {
                    a.s5 = Some((s, sdt::SSDT, phys));
                    return;
                }
            }
            Ok(_) => a.notes.push(Note::table(sdt::SSDT, phys, "bad_checksum", 0)),
            Err(e) => a.notes.push(Note::table(sdt::SSDT, phys, e.name(), detail(&e))),
        }
    }
}

fn detail(e: &TableError) -> u64 {
    match e {
        TableError::TooShort(l) | TableError::TooLong(l) => *l as u64,
        TableError::BadSignature(s) => u32::from_le_bytes(*s) as u64,
        _ => 0,
    }
}

impl Header {
    pub fn signature_text(&self) -> alloc::string::String {
        crate::bytes::text(&self.signature)
    }
}
