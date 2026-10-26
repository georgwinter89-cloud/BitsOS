//! The BootInfo the new kernel gets (BootInfo v1, crate `bootinfo`'s writer): every record of the running one in
//! its order and unchanged (CONFIG, STICK, the KEEP records CRASH_PAGE / TRAMPOLINE / RELOAD_STUB, framebuffer,
//! RSDP, generation, ..., and tags this kernel does not know), except the ones that describe the new kernel:
//! KERNEL_IMAGE, the initramfs MODULE, PAGE_TABLES, TSC (the running kernel's calibrated rate), RELOAD (+1) and
//! the memory map (last, as bitsboot writes it). Pure.
//! RB-10: [`refresh`] rewrites the staged BootInfo right before the jump with the stick's CONFIG.TXT as it is
//! then and the running generation's reason as its markers say (`crate::refresh` decides both).

use alloc::vec::Vec;
use bootinfo::{BootInfo, Generation, KernelImage, MemDesc, Module, PageTables, Reload, Tsc, WriteError, Writer, module_kind, tag, tsc_source};

pub struct NewBoot<'a> {
    pub image: KernelImage,
    /// The new initramfs `(phys, size)`; `None` passes the running one on.
    pub initrd: Option<(u64, u64)>,
    pub tables: PageTables,
    pub tsc: Tsc,
    pub map: &'a [MemDesc],
}

/// The BootInfo's capacity: the size of the `KEEP` block bitsboot reserved (header field 20..24).
pub fn capacity(old: &BootInfo<'_>) -> usize {
    let b = old.as_bytes();
    u32::from_le_bytes([b[20], b[21], b[22], b[23]]) as usize
}

pub fn next_counter(old: &BootInfo<'_>) -> u32 {
    old.reload().map(|r| r.counter).unwrap_or(0).saturating_add(1)
}

/// The TSC record: the running kernel's rate. A rate first measured by a kernel keeps its origin (source and the
/// Reload number of the kernel that measured it); a cold-boot calibration, or a handed-on rate this kernel
/// corrected against the HPET (`measured_here`, `time::rate_measured_here`, INT-A1e), is recorded as measured
/// by this kernel (`time` calibrates against the PIT in G0).
pub fn next_tsc(old: &BootInfo<'_>, running_hz: u64, running_source: u32, measured_here: bool) -> Tsc {
    let o = old.tsc().unwrap_or_default();
    let counter = old.reload().map(|r| r.counter).unwrap_or(0);
    if o.calibrated_hz != 0 && !measured_here {
        Tsc { calibrated_hz: running_hz, ..o }
    } else {
        // CP-5 change request 4b (INT-A1): the source this kernel calibrated against (`time::source()`: HPET on
        // the Mini-PC), not always the PIT; NONE (no calibration recorded) keeps G0's PIT.
        let source = if running_source == tsc_source::NONE { tsc_source::PIT } else { running_source };
        Tsc { hint_hz: o.hint_hz, calibrated_hz: running_hz, source, calibrated_at_reload: counter }
    }
}

/// The old initramfs ranges `(start, end)` a new initramfs replaces.
pub fn old_initrds(old: &BootInfo<'_>) -> Vec<(u64, u64)> {
    old.modules().filter(|m| m.kind == module_kind::INITRAMFS).map(|m| (m.phys, m.phys + m.size)).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildError {
    /// The buffer of `capacity` bytes could not be allocated.
    NoMemory { capacity: usize },
    /// The records do not fit `capacity`.
    Write(WriteError),
    /// [`refresh`]: the staged bytes are no BootInfo (cannot happen for bytes [`build`] wrote).
    Unreadable,
}

impl From<WriteError> for BuildError {
    fn from(e: WriteError) -> Self {
        BuildError::Write(e)
    }
}

/// Writes the new BootInfo into a buffer of `capacity` bytes (the KEEP block's size, which the caller checked
/// against the memory map); returns the bytes up to its total size.
pub fn build(old: &BootInfo<'_>, n: &NewBoot<'_>, capacity: usize) -> Result<Vec<u8>, BuildError> {
    let mut buf = Vec::new();
    buf.try_reserve_exact(capacity).map_err(|_| BuildError::NoMemory { capacity })?;
    buf.resize(capacity, 0);
    let mut w = Writer::new(&mut buf)?;
    let mut done_image = false;
    let mut done_initrd = n.initrd.is_none();
    let mut done_tables = false;
    let mut done_tsc = false;
    let mut done_reload = false;
    let counter = next_counter(old);
    let new_initrd = |w: &mut Writer<'_>, (phys, size): (u64, u64)| {
        w.module(&Module { kind: module_kind::INITRAMFS, phys, size, name: b"INITRD" })
    };
    for r in old.records() {
        match r.tag {
            tag::KERNEL_IMAGE if !done_image => {
                w.kernel_image(n.image)?;
                done_image = true;
            }
            tag::KERNEL_IMAGE => {}
            tag::MODULE if n.initrd.is_some() && bootinfo_module_kind(r.payload) == Some(module_kind::INITRAMFS) => {
                if let (false, Some(i)) = (done_initrd, n.initrd) {
                    new_initrd(&mut w, i)?;
                    done_initrd = true;
                }
            }
            tag::PAGE_TABLES if !done_tables => {
                w.page_tables(n.tables)?;
                done_tables = true;
            }
            tag::PAGE_TABLES => {}
            tag::TSC if !done_tsc => {
                w.tsc(n.tsc)?;
                done_tsc = true;
            }
            tag::TSC => {}
            tag::RELOAD if !done_reload => {
                w.reload(Reload { counter })?;
                done_reload = true;
            }
            tag::RELOAD => {}
            tag::MEMORY_MAP => {}
            t => w.record(t, r.payload)?,
        }
    }
    if !done_image {
        w.kernel_image(n.image)?;
    }
    if let (false, Some(i)) = (done_initrd, n.initrd) {
        new_initrd(&mut w, i)?;
    }
    if !done_tables {
        w.page_tables(n.tables)?;
    }
    if !done_tsc {
        w.tsc(n.tsc)?;
    }
    if !done_reload {
        w.reload(Reload { counter })?;
    }
    w.memory_map(n.map.iter().copied())?;
    let len = w.len();
    buf.truncate(len);
    Ok(buf)
}

/// RB-10: the staged BootInfo `staged` (written by [`build`]) again, with the CONFIG record's text replaced by
/// `config` and the GENERATION record by `generation` where they are `Some`; a CONFIG record the staged one
/// lacks goes before the memory map. Every other record keeps its bytes and its place.
pub fn refresh(staged: &[u8], capacity: usize, config: Option<&[u8]>, generation: Option<Generation>) -> Result<Vec<u8>, BuildError> {
    let old = BootInfo::parse(staged).map_err(|_| BuildError::Unreadable)?;
    let mut buf = Vec::new();
    buf.try_reserve_exact(capacity).map_err(|_| BuildError::NoMemory { capacity })?;
    buf.resize(capacity, 0);
    let mut w = Writer::new(&mut buf)?;
    let mut config_done = config.is_none();
    // RB-9: a Reload into another generation hands its record on also when the running kernel had none.
    let mut generation_done = generation.is_none();
    for r in old.records() {
        if r.tag == tag::GENERATION {
            generation_done = true;
        }
        match (r.tag, config, generation) {
            (tag::CONFIG, Some(c), _) => {
                if !config_done {
                    w.config(c)?;
                    config_done = true;
                }
            }
            (tag::GENERATION, _, Some(g)) => w.generation(g)?,
            (tag::MEMORY_MAP, Some(c), _) if !config_done => {
                w.config(c)?;
                config_done = true;
                w.record(r.tag, r.payload)?;
            }
            (t, _, _) => w.record(t, r.payload)?,
        }
    }
    if let (false, Some(c)) = (config_done, config) {
        w.config(c)?;
    }
    if let (false, Some(g)) = (generation_done, generation) {
        w.generation(g)?;
    }
    let len = w.len();
    buf.truncate(len);
    Ok(buf)
}

fn bootinfo_module_kind(p: &[u8]) -> Option<u32> {
    (p.len() >= 4).then(|| u32::from_le_bytes([p[0], p[1], p[2], p[3]]))
}
