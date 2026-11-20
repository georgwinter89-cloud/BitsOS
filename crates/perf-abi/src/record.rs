//! Record layouts of the ring buffer (linux-7.3.1/include/uapi/linux/perf_event.h:904-1091): the kernel builds
//! them with `Out` (into a fixed buffer, no allocation: samples are written in interrupt context), readers
//! (bitsprof, the ktests) take them apart with `Records`.

use crate::*;

/// Every record starts with `struct perf_event_header`: type, misc, size (bytes, a multiple of 8).
pub const HEADER: usize = 8;

/// A record being built into a caller's buffer; `None` from `finish` when it did not fit.
pub struct Out<'a> {
    buf: &'a mut [u8],
    len: usize,
    over: bool,
}

impl<'a> Out<'a> {
    pub fn new(buf: &'a mut [u8], kind: u32, misc: u16) -> Out<'a> {
        let mut o = Out { buf, len: 0, over: false };
        o.u32(kind);
        o.u16(misc);
        o.u16(0);
        o
    }
    fn put(&mut self, b: &[u8]) {
        if self.over || self.len + b.len() > self.buf.len() {
            self.over = true;
            return;
        }
        self.buf[self.len..self.len + b.len()].copy_from_slice(b);
        self.len += b.len();
    }
    pub fn u16(&mut self, v: u16) {
        self.put(&v.to_le_bytes());
    }
    pub fn u32(&mut self, v: u32) {
        self.put(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.put(&v.to_le_bytes());
    }
    /// A NUL-terminated string padded with zeros to a multiple of 8 (`ALIGN(strlen + 1, 8)`).
    pub fn cstr(&mut self, s: &[u8]) {
        let s = &s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())];
        self.put(s);
        let pad = (s.len() + 1).next_multiple_of(8) - s.len();
        for _ in 0..pad {
            self.put(&[0]);
        }
    }
    /// Writes the size into the header; the record's bytes.
    pub fn finish(self) -> Option<&'a [u8]> {
        if self.over || self.len % 8 != 0 || self.len > u16::MAX as usize {
            return None;
        }
        let n = self.len as u16;
        self.buf[6..8].copy_from_slice(&n.to_le_bytes());
        Some(&self.buf[..self.len])
    }
}

/// What a sample-id trailer and a sample's id fields need (`perf_event_header__init_id`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ident {
    pub pid: u32,
    pub tid: u32,
    pub time: u64,
    pub id: u64,
    pub stream_id: u64,
    pub cpu: u32,
}

/// The `sample_id_all` trailer (`__perf_event__output_id_sample`): pid/tid, time, id, stream_id, cpu,
/// identifier, each when its sample bit is set.
pub fn trailer(o: &mut Out<'_>, sample_type: u64, id: &Ident) {
    if sample_type & SAMPLE_TID != 0 {
        o.u32(id.pid);
        o.u32(id.tid);
    }
    if sample_type & SAMPLE_TIME != 0 {
        o.u64(id.time);
    }
    if sample_type & SAMPLE_ID != 0 {
        o.u64(id.id);
    }
    if sample_type & SAMPLE_STREAM_ID != 0 {
        o.u64(id.stream_id);
    }
    if sample_type & SAMPLE_CPU != 0 {
        o.u32(id.cpu);
        o.u32(0);
    }
    if sample_type & SAMPLE_IDENTIFIER != 0 {
        o.u64(id.id);
    }
}

/// The value part of a read (`perf_output_read_one`, and read(2) without FORMAT_GROUP).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadValue {
    pub value: u64,
    pub enabled: u64,
    pub running: u64,
    pub id: u64,
    pub lost: u64,
}

/// The read values in a sample: `perf_output_read_one`, or with FORMAT_GROUP `perf_output_read_group`: `v` is
/// the group leader's, `siblings` the other members' (the times are the leader's).
pub fn read_value(o: &mut Out<'_>, read_format: u64, v: &ReadValue, siblings: &[ReadValue]) {
    if read_format & FORMAT_GROUP != 0 {
        o.u64(1 + siblings.len() as u64);
        if read_format & FORMAT_TOTAL_TIME_ENABLED != 0 {
            o.u64(v.enabled);
        }
        if read_format & FORMAT_TOTAL_TIME_RUNNING != 0 {
            o.u64(v.running);
        }
        for m in core::iter::once(v).chain(siblings) {
            o.u64(m.value);
            if read_format & FORMAT_ID != 0 {
                o.u64(m.id);
            }
            if read_format & FORMAT_LOST != 0 {
                o.u64(m.lost);
            }
        }
        return;
    }
    o.u64(v.value);
    if read_format & FORMAT_TOTAL_TIME_ENABLED != 0 {
        o.u64(v.enabled);
    }
    if read_format & FORMAT_TOTAL_TIME_RUNNING != 0 {
        o.u64(v.running);
    }
    if read_format & FORMAT_ID != 0 {
        o.u64(v.id);
    }
    if read_format & FORMAT_LOST != 0 {
        o.u64(v.lost);
    }
}

/// One PERF_RECORD_SAMPLE in Linux's field order (`perf_output_sample`).
pub struct Sample<'c> {
    pub misc: u16,
    pub ip: u64,
    pub ident: Ident,
    pub addr: u64,
    pub period: u64,
    pub read: ReadValue,
    /// The other members of the group (FORMAT_GROUP), the leader's values being `read`.
    pub siblings: &'c [ReadValue],
    pub callchain: &'c [u64],
}

pub fn sample<'a>(buf: &'a mut [u8], sample_type: u64, read_format: u64, s: &Sample<'_>) -> Option<&'a [u8]> {
    let mut o = Out::new(buf, RECORD_SAMPLE, s.misc);
    let t = sample_type;
    if t & SAMPLE_IDENTIFIER != 0 {
        o.u64(s.ident.id);
    }
    if t & SAMPLE_IP != 0 {
        o.u64(s.ip);
    }
    if t & SAMPLE_TID != 0 {
        o.u32(s.ident.pid);
        o.u32(s.ident.tid);
    }
    if t & SAMPLE_TIME != 0 {
        o.u64(s.ident.time);
    }
    if t & SAMPLE_ADDR != 0 {
        o.u64(s.addr);
    }
    if t & SAMPLE_ID != 0 {
        o.u64(s.ident.id);
    }
    if t & SAMPLE_STREAM_ID != 0 {
        o.u64(s.ident.stream_id);
    }
    if t & SAMPLE_CPU != 0 {
        o.u32(s.ident.cpu);
        o.u32(0);
    }
    if t & SAMPLE_PERIOD != 0 {
        o.u64(s.period);
    }
    if t & SAMPLE_READ != 0 {
        read_value(&mut o, read_format, &s.read, s.siblings);
    }
    if t & SAMPLE_CALLCHAIN != 0 {
        o.u64(s.callchain.len() as u64);
        for &ip in s.callchain {
            o.u64(ip);
        }
    }
    o.finish()
}

/// PERF_RECORD_LOST: id, lost.
pub fn lost<'a>(buf: &'a mut [u8], sample_type: u64, id_all: bool, ident: &Ident, n: u64) -> Option<&'a [u8]> {
    let mut o = Out::new(buf, RECORD_LOST, 0);
    o.u64(ident.id);
    o.u64(n);
    if id_all {
        trailer(&mut o, sample_type, ident);
    }
    o.finish()
}

/// PERF_RECORD_COMM: pid, tid, the name (MISC_COMM_EXEC when an exec set it).
pub fn comm<'a>(buf: &'a mut [u8], sample_type: u64, id_all: bool, ident: &Ident, name: &[u8], exec: bool) -> Option<&'a [u8]> {
    let mut o = Out::new(buf, RECORD_COMM, if exec { MISC_COMM_EXEC } else { 0 });
    o.u32(ident.pid);
    o.u32(ident.tid);
    o.cstr(name);
    if id_all {
        trailer(&mut o, sample_type, ident);
    }
    o.finish()
}

/// PERF_RECORD_EXIT and PERF_RECORD_FORK: pid, ppid, tid, ptid, time.
pub fn task<'a>(buf: &'a mut [u8], kind: u32, sample_type: u64, id_all: bool, ident: &Ident, ppid: u32, ptid: u32) -> Option<&'a [u8]> {
    let mut o = Out::new(buf, kind, 0);
    o.u32(ident.pid);
    o.u32(ppid);
    o.u32(ident.tid);
    o.u32(ptid);
    o.u64(ident.time);
    if id_all {
        trailer(&mut o, sample_type, ident);
    }
    o.finish()
}

/// A mapping for PERF_RECORD_MMAP2 (and MMAP: the same without device, inode, prot and flags).
#[derive(Clone, Copy, Debug, Default)]
pub struct Map<'n> {
    pub addr: u64,
    pub len: u64,
    pub pgoff: u64,
    pub maj: u32,
    pub min: u32,
    pub ino: u64,
    pub prot: u32,
    pub flags: u32,
    pub name: &'n [u8],
}

pub fn mmap2<'a>(buf: &'a mut [u8], sample_type: u64, id_all: bool, ident: &Ident, misc: u16, m: &Map<'_>) -> Option<&'a [u8]> {
    let mut o = Out::new(buf, RECORD_MMAP2, misc);
    o.u32(ident.pid);
    o.u32(ident.tid);
    o.u64(m.addr);
    o.u64(m.len);
    o.u64(m.pgoff);
    o.u32(m.maj);
    o.u32(m.min);
    o.u64(m.ino);
    o.u64(0); // ino_generation
    o.u32(m.prot);
    o.u32(m.flags);
    o.cstr(m.name);
    if id_all {
        trailer(&mut o, sample_type, ident);
    }
    o.finish()
}

pub fn mmap<'a>(buf: &'a mut [u8], sample_type: u64, id_all: bool, ident: &Ident, misc: u16, m: &Map<'_>) -> Option<&'a [u8]> {
    let mut o = Out::new(buf, RECORD_MMAP, misc);
    o.u32(ident.pid);
    o.u32(ident.tid);
    o.u64(m.addr);
    o.u64(m.len);
    o.u64(m.pgoff);
    o.cstr(m.name);
    if id_all {
        trailer(&mut o, sample_type, ident);
    }
    o.finish()
}

// ---------------------------------------------------------------------------------------------------------
// Reading

/// A record as a reader sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record<'a> {
    Sample(Parsed<'a>),
    Mmap2 { pid: u32, tid: u32, addr: u64, len: u64, pgoff: u64, maj: u32, min: u32, ino: u64, prot: u32, flags: u32, name: &'a [u8] },
    Mmap { pid: u32, tid: u32, addr: u64, len: u64, pgoff: u64, name: &'a [u8] },
    Comm { pid: u32, tid: u32, name: &'a [u8], exec: bool },
    Exit { pid: u32, ppid: u32, tid: u32, ptid: u32, time: u64 },
    Fork { pid: u32, ppid: u32, tid: u32, ptid: u32, time: u64 },
    Lost { id: u64, lost: u64 },
    Other { kind: u32 },
}

/// A sample's fields (those its sample type has; the others are 0).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Parsed<'a> {
    pub misc: u16,
    pub ip: u64,
    pub pid: u32,
    pub tid: u32,
    pub time: u64,
    pub id: u64,
    pub cpu: u32,
    pub period: u64,
    /// The raw callchain words (8 bytes each, markers included).
    pub chain: &'a [u8],
}

impl Parsed<'_> {
    pub fn chain_len(&self) -> usize {
        self.chain.len() / 8
    }
    pub fn chain_at(&self, i: usize) -> u64 {
        rd64(self.chain, i * 8).unwrap_or(0)
    }
}

fn rd32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
fn rd64(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}
fn cstr_at(b: &[u8], o: usize) -> &[u8] {
    let s = b.get(o..).unwrap_or(&[]);
    &s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())]
}

/// Decodes one record (`rec` is exactly its bytes) for an event of `sample_type` and `read_format`.
pub fn parse(rec: &[u8], sample_type: u64, read_format: u64) -> Option<Record<'_>> {
    let kind = rd32(rec, 0)?;
    let misc = u16::from_le_bytes(rec.get(4..6)?.try_into().ok()?);
    let b = &rec[HEADER..];
    Some(match kind {
        RECORD_SAMPLE => {
            let mut p = Parsed { misc, ..Default::default() };
            let mut o = 0;
            let t = sample_type;
            let take = |o: &mut usize| {
                let v = rd64(b, *o);
                *o += 8;
                v
            };
            if t & SAMPLE_IDENTIFIER != 0 {
                p.id = take(&mut o)?;
            }
            if t & SAMPLE_IP != 0 {
                p.ip = take(&mut o)?;
            }
            if t & SAMPLE_TID != 0 {
                let v = take(&mut o)?;
                (p.pid, p.tid) = (v as u32, (v >> 32) as u32);
            }
            if t & SAMPLE_TIME != 0 {
                p.time = take(&mut o)?;
            }
            if t & SAMPLE_ADDR != 0 {
                take(&mut o)?;
            }
            if t & SAMPLE_ID != 0 {
                p.id = take(&mut o)?;
            }
            if t & SAMPLE_STREAM_ID != 0 {
                take(&mut o)?;
            }
            if t & SAMPLE_CPU != 0 {
                p.cpu = take(&mut o)? as u32;
            }
            if t & SAMPLE_PERIOD != 0 {
                p.period = take(&mut o)?;
            }
            if t & SAMPLE_READ != 0 {
                let times = (read_format & (FORMAT_TOTAL_TIME_ENABLED | FORMAT_TOTAL_TIME_RUNNING)).count_ones() as usize;
                let per = 1 + (read_format & (FORMAT_ID | FORMAT_LOST)).count_ones() as usize;
                if read_format & FORMAT_GROUP != 0 {
                    let nr = take(&mut o)? as usize;
                    o = o.checked_add(8 * (times + nr.checked_mul(per)?))?;
                } else {
                    o += 8 * (times + per);
                }
            }
            if t & SAMPLE_CALLCHAIN != 0 {
                let n = take(&mut o)? as usize;
                p.chain = b.get(o..o.checked_add(n.checked_mul(8)?)?)?;
            }
            Record::Sample(p)
        }
        RECORD_MMAP2 => Record::Mmap2 {
            pid: rd32(b, 0)?,
            tid: rd32(b, 4)?,
            addr: rd64(b, 8)?,
            len: rd64(b, 16)?,
            pgoff: rd64(b, 24)?,
            maj: rd32(b, 32)?,
            min: rd32(b, 36)?,
            ino: rd64(b, 40)?,
            prot: rd32(b, 56)?,
            flags: rd32(b, 60)?,
            name: cstr_at(b, 64),
        },
        RECORD_MMAP => Record::Mmap { pid: rd32(b, 0)?, tid: rd32(b, 4)?, addr: rd64(b, 8)?, len: rd64(b, 16)?, pgoff: rd64(b, 24)?, name: cstr_at(b, 32) },
        RECORD_COMM => Record::Comm { pid: rd32(b, 0)?, tid: rd32(b, 4)?, name: cstr_at(b, 8), exec: misc & MISC_COMM_EXEC != 0 },
        RECORD_EXIT | RECORD_FORK => {
            let (pid, ppid, tid, ptid, time) = (rd32(b, 0)?, rd32(b, 4)?, rd32(b, 8)?, rd32(b, 12)?, rd64(b, 16)?);
            if kind == RECORD_EXIT { Record::Exit { pid, ppid, tid, ptid, time } } else { Record::Fork { pid, ppid, tid, ptid, time } }
        }
        RECORD_LOST => Record::Lost { id: rd64(b, 0)?, lost: rd64(b, 8)? },
        k => Record::Other { kind: k },
    })
}

/// The records of a contiguous byte run (as read out of the ring, wrap already undone). Stops at a truncated
/// or malformed header.
pub struct Records<'a> {
    pub bytes: &'a [u8],
    pub at: usize,
}

impl<'a> Iterator for Records<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        let size = u16::from_le_bytes(self.bytes.get(self.at + 6..self.at + 8)?.try_into().ok()?) as usize;
        if size < HEADER || size % 8 != 0 {
            return None;
        }
        let r = self.bytes.get(self.at..self.at + size)?;
        self.at += size;
        Some(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_round_trip() {
        let mut buf = [0u8; 512];
        let st = SAMPLE_IP | SAMPLE_TID | SAMPLE_TIME | SAMPLE_CPU | SAMPLE_PERIOD | SAMPLE_CALLCHAIN | SAMPLE_IDENTIFIER;
        let chain = [CONTEXT_KERNEL, 0xffff_ffff_8000_1000, CONTEXT_USER, 0x40_1000];
        let s = Sample {
            misc: MISC_KERNEL,
            ip: 0xffff_ffff_8000_1000,
            ident: Ident { pid: 7, tid: 8, time: 99, id: 3, stream_id: 3, cpu: 2 },
            addr: 0,
            period: 1_000_000,
            read: ReadValue::default(),
            siblings: &[],
            callchain: &chain,
        };
        let r = sample(&mut buf, st, 0, &s).unwrap();
        assert_eq!(r.len(), 8 + 8 * 6 + 8 + 32);
        let Some(Record::Sample(p)) = parse(r, st, 0) else { panic!() };
        assert_eq!((p.ip, p.pid, p.tid, p.time, p.cpu, p.period, p.id, p.misc), (s.ip, 7, 8, 99, 2, 1_000_000, 3, MISC_KERNEL));
        assert_eq!(p.chain_len(), 4);
        assert_eq!(p.chain_at(2), CONTEXT_USER);
    }

    #[test]
    fn sample_with_group_read() {
        let mut buf = [0u8; 256];
        let st = SAMPLE_IP | SAMPLE_READ | SAMPLE_PERIOD;
        let rf = FORMAT_GROUP | FORMAT_TOTAL_TIME_ENABLED | FORMAT_ID;
        let read = ReadValue { value: 5, enabled: 9, running: 9, id: 4, lost: 0 };
        let sib = [ReadValue { value: 6, id: 5, ..Default::default() }];
        let s = Sample { misc: MISC_USER, ip: 0x1000, ident: Ident::default(), addr: 0, period: 7, read, siblings: &sib, callchain: &[] };
        let r = sample(&mut buf, st, rf, &s).unwrap();
        // header, ip, period, nr, enabled, (value, id) twice
        assert_eq!(r.len(), 8 + 8 * 8);
        let Some(Record::Sample(p)) = parse(r, st, rf) else { panic!() };
        assert_eq!((p.ip, p.period), (0x1000, 7));
    }

    #[test]
    fn side_band_records() {
        let mut buf = [0u8; 256];
        let st = SAMPLE_TID | SAMPLE_TIME;
        let id = Ident { pid: 10, tid: 11, time: 5, ..Default::default() };
        let r = comm(&mut buf, st, true, &id, b"busy\0junk", true).unwrap();
        // header + pid/tid + "busy\0" padded to 8 + trailer 16
        assert_eq!(r.len(), 8 + 8 + 8 + 16);
        assert_eq!(parse(r, st, 0), Some(Record::Comm { pid: 10, tid: 11, name: b"busy", exec: true }));
        let r = comm(&mut buf, st, false, &id, b"12345678", false).unwrap();
        assert_eq!(r.len(), 8 + 8 + 16);
        let r = task(&mut buf, RECORD_EXIT, st, false, &id, 1, 1).unwrap();
        assert_eq!(parse(r, st, 0), Some(Record::Exit { pid: 10, ppid: 1, tid: 11, ptid: 1, time: 5 }));
        let m = Map { addr: 0x1000, len: 0x2000, pgoff: 0, maj: 8, min: 1, ino: 42, prot: 5, flags: 2, name: b"/bin/x" };
        let r = mmap2(&mut buf, st, false, &id, MISC_USER, &m).unwrap();
        match parse(r, st, 0) {
            Some(Record::Mmap2 { addr, len, ino, name, prot, .. }) => assert_eq!((addr, len, ino, name, prot), (0x1000, 0x2000, 42, &b"/bin/x"[..], 5)),
            x => panic!("{x:?}"),
        }
        let r = lost(&mut buf, st, false, &id, 17).unwrap();
        assert_eq!(parse(r, st, 0), Some(Record::Lost { id: 0, lost: 17 }));
    }

    #[test]
    fn too_small_buffer() {
        let mut buf = [0u8; 16];
        let id = Ident::default();
        assert!(comm(&mut buf, 0, false, &id, b"a long name here", false).is_none());
    }

    #[test]
    fn records_iterator_stops_on_garbage() {
        let mut a = [0u8; 64];
        let mut b1 = [0u8; 64];
        let r = lost(&mut b1, 0, false, &Ident::default(), 1).unwrap();
        a[..r.len()].copy_from_slice(r);
        let n = Records { bytes: &a, at: 0 }.count();
        assert_eq!(n, 1);
    }
}
