//! The new kernel's ELF, checked and loaded while it streams in (only its segments are kept: the 14 MB file of a
//! debug kernel is mostly debug info that Reload never stores). Pure: host-tested, and the same code runs in the
//! kernel. Checks: those of bitsboot's `elf.rs` (the loader the new kernel would get on a cold boot) plus entry
//! inside an executable segment and no overlapping segments.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use bootinfo::layout::KERNEL_BASE;

pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
const PT_LOAD: u32 = 1;
const PHDR_SIZE: usize = 56;
const PAGE: u64 = 4096;
/// Hostile-input bound (bitsboot's): a kernel spanning more than 1 GiB is a broken header, not a kernel.
pub const MAX_SPAN: u64 = 1 << 30;
/// Hostile-input bound: the ELF header and the program headers must lie in the first 64 KiB of the file.
pub const HEAD_MAX: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElfError {
    NotElf,
    Not64BitLittleEndian,
    NotX86_64,
    NotExecutable,
    BadHeaderSize,
    HeadersTooFar { end: u64 },
    NoLoadSegments,
    BelowKernelBase { vaddr: u64 },
    FileBytesOverMemory,
    Overflow,
    SpanTooLarge { span: u64 },
    EntryNotExecutable { entry: u64 },
    SegmentsOverlap { a: u64, b: u64 },
    /// The file ended before every segment's bytes arrived.
    Truncated { have: u64, need: u64 },
    /// The place the segments go refused (no memory for the image).
    Sink(String),
}

impl ElfError {
    pub fn describe(&self) -> String {
        match self {
            ElfError::NotElf => "not an ELF file (bad magic)".into(),
            ElfError::Not64BitLittleEndian => "not a 64-bit little-endian ELF".into(),
            ElfError::NotX86_64 => "not built for x86_64".into(),
            ElfError::NotExecutable => "not an executable (ELF type is not EXEC)".into(),
            ElfError::BadHeaderSize => "unexpected ELF or program header size".into(),
            ElfError::HeadersTooFar { end } => format!("the program headers end at byte {end}, beyond the first {HEAD_MAX}"),
            ElfError::NoLoadSegments => "no loadable segments".into(),
            ElfError::BelowKernelBase { vaddr } => format!("a segment at {vaddr:#x} lies below the kernel base {KERNEL_BASE:#x}"),
            ElfError::FileBytesOverMemory => "a segment has more file bytes than memory bytes".into(),
            ElfError::Overflow => "a segment's address or size overflows".into(),
            ElfError::SpanTooLarge { span } => format!("the kernel spans {span} bytes (more than 1 GiB)"),
            ElfError::EntryNotExecutable { entry } => format!("the entry {entry:#x} lies in no executable segment"),
            ElfError::SegmentsOverlap { a, b } => format!("segments at {a:#x} and {b:#x} overlap"),
            ElfError::Truncated { have, need } => format!("the file ended after {have} bytes; its segments need {need}"),
            ElfError::Sink(s) => s.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub vaddr: u64,
    pub offset: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub flags: u32,
}

/// The image as it will lie in memory: `span` bytes from the page-aligned `lo`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub entry: u64,
    pub lo: u64,
    pub span: u64,
    pub segs: Vec<Segment>,
}

impl Layout {
    /// The file size the segments need.
    pub fn file_end(&self) -> u64 {
        self.segs.iter().map(|s| s.offset + s.filesz).max().unwrap_or(0)
    }
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

/// Parses the ELF header and program headers from the start of the file. `Ok(None)`: more bytes are needed.
pub fn parse_head(b: &[u8]) -> Result<Option<Layout>, ElfError> {
    if b.len() >= 4 && b[0..4] != *b"\x7fELF" {
        return Err(ElfError::NotElf);
    }
    if b.len() < 64 {
        return Ok(None);
    }
    if b[4] != 2 || b[5] != 1 {
        return Err(ElfError::Not64BitLittleEndian);
    }
    if u16_at(b, 18) != 0x3E {
        return Err(ElfError::NotX86_64);
    }
    if u16_at(b, 16) != 2 {
        return Err(ElfError::NotExecutable);
    }
    let (phoff, phentsize, phnum) = (u64_at(b, 32), u16_at(b, 54) as usize, u16_at(b, 56) as u64);
    if u16_at(b, 52) != 64 || phentsize != PHDR_SIZE {
        return Err(ElfError::BadHeaderSize);
    }
    let end = phoff.checked_add(phnum * PHDR_SIZE as u64).ok_or(ElfError::Overflow)?;
    if end > HEAD_MAX as u64 {
        return Err(ElfError::HeadersTooFar { end });
    }
    if (b.len() as u64) < end {
        return Ok(None);
    }
    let mut segs = Vec::new();
    for i in 0..phnum as usize {
        let h = &b[phoff as usize + i * PHDR_SIZE..][..PHDR_SIZE];
        if u32_at(h, 0) != PT_LOAD {
            continue;
        }
        let s = Segment { flags: u32_at(h, 4), offset: u64_at(h, 8), vaddr: u64_at(h, 16), filesz: u64_at(h, 32), memsz: u64_at(h, 40) };
        if s.memsz == 0 {
            continue;
        }
        if s.filesz > s.memsz {
            return Err(ElfError::FileBytesOverMemory);
        }
        s.offset.checked_add(s.filesz).ok_or(ElfError::Overflow)?;
        if s.vaddr < KERNEL_BASE {
            return Err(ElfError::BelowKernelBase { vaddr: s.vaddr });
        }
        s.vaddr.checked_add(s.memsz).and_then(|e| e.checked_next_multiple_of(PAGE)).ok_or(ElfError::Overflow)?;
        segs.push(s);
    }
    if segs.is_empty() {
        return Err(ElfError::NoLoadSegments);
    }
    segs.sort_by_key(|s| s.vaddr);
    for w in segs.windows(2) {
        if w[0].vaddr + w[0].memsz > w[1].vaddr {
            return Err(ElfError::SegmentsOverlap { a: w[0].vaddr, b: w[1].vaddr });
        }
    }
    let lo = segs[0].vaddr & !(PAGE - 1);
    let hi = segs.iter().map(|s| (s.vaddr + s.memsz).next_multiple_of(PAGE)).max().unwrap_or(lo);
    let span = hi - lo;
    if span > MAX_SPAN {
        return Err(ElfError::SpanTooLarge { span });
    }
    let entry = u64_at(b, 24);
    if !segs.iter().any(|s| s.flags & PF_X != 0 && entry >= s.vaddr && entry < s.vaddr + s.memsz) {
        return Err(ElfError::EntryNotExecutable { entry });
    }
    Ok(Some(Layout { entry, lo, span, segs }))
}

/// Where the segments go: `layout` once the headers are known (allocate and zero `span` bytes), then `write`
/// at offsets inside the span.
pub trait ImageSink {
    fn layout(&mut self, l: &Layout) -> Result<(), String>;
    fn write(&mut self, span_off: u64, data: &[u8]) -> Result<(), String>;
}

/// Feeds the file piece by piece; keeps only the head until the headers are complete.
#[derive(Default)]
pub struct ElfStream {
    head: Vec<u8>,
    layout: Option<Layout>,
    received: u64,
}

impl ElfStream {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, data: &[u8], sink: &mut dyn ImageSink) -> Result<(), ElfError> {
        if self.layout.is_some() {
            self.write_range(self.received, data, sink)?;
            self.received += data.len() as u64;
            return Ok(());
        }
        // Only what the headers need is buffered; the rest of this piece is written once the layout is known.
        let want = HEAD_MAX.saturating_sub(self.head.len()).min(data.len());
        self.head.extend_from_slice(&data[..want]);
        let Some(l) = parse_head(&self.head)? else {
            if want < data.len() {
                // HEAD_MAX bytes and still incomplete: parse_head reports HeadersTooFar before this.
                return Err(ElfError::HeadersTooFar { end: HEAD_MAX as u64 });
            }
            self.received += data.len() as u64;
            return Ok(());
        };
        sink.layout(&l).map_err(ElfError::Sink)?;
        self.layout = Some(l);
        let head = core::mem::take(&mut self.head);
        let start = self.received + want as u64 - head.len() as u64;
        self.write_range(start, &head, sink)?;
        self.received = start + head.len() as u64;
        let rest = &data[want..];
        self.write_range(self.received, rest, sink)?;
        self.received += rest.len() as u64;
        Ok(())
    }

    fn write_range(&self, file_off: u64, data: &[u8], sink: &mut dyn ImageSink) -> Result<(), ElfError> {
        let Some(l) = &self.layout else { return Ok(()) };
        let end = file_off + data.len() as u64;
        for s in &l.segs {
            let (a, b) = (file_off.max(s.offset), end.min(s.offset + s.filesz));
            if a < b {
                let dest = s.vaddr - l.lo + (a - s.offset);
                sink.write(dest, &data[(a - file_off) as usize..(b - file_off) as usize]).map_err(ElfError::Sink)?;
            }
        }
        Ok(())
    }

    /// The end of the file: every segment byte must have arrived.
    pub fn finish(self) -> Result<Layout, ElfError> {
        let have = self.received.max(self.head.len() as u64);
        let Some(l) = self.layout else {
            return Err(if have < 4 { ElfError::NotElf } else { ElfError::Truncated { have, need: 64 } });
        };
        let need = l.file_end();
        if have < need {
            return Err(ElfError::Truncated { have, need });
        }
        Ok(l)
    }
}
