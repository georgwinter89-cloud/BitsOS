//! A simulated Bulk-Only stick for host tests: a SCSI disk model per LUN (contents as a pattern function plus the
//! blocks written), the BOT state machine of a device (CBW, data, CSW, halts, invalid CBW until reset recovery)
//! and injected faults. Time is virtual (advanced by sleeps and timeouts).

use crate::link::{Link, CBW_TIMEOUT_NS, CSW_TIMEOUT_NS};
use crate::scsi::{op, Sense};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use std::sync::Mutex;
use usb_core::{Buffer, Request, UsbError, XferMem, XferResult};

pub struct Mem(Mutex<Vec<u8>>);

impl XferMem for Mem {
    fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }
    fn read(&self, off: usize, out: &mut [u8]) -> bool {
        let v = self.0.lock().unwrap();
        match v.get(off..off.saturating_add(out.len())) {
            Some(s) => {
                out.copy_from_slice(s);
                true
            }
            None => false,
        }
    }
    fn write(&self, off: usize, data: &[u8]) -> bool {
        let mut v = self.0.lock().unwrap();
        match v.get_mut(off..off.saturating_add(data.len())) {
            Some(s) => {
                s.copy_from_slice(data);
                true
            }
            None => false,
        }
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The byte at `off` of a never-written disk.
pub fn pattern(off: u64) -> u8 {
    ((off >> 9).wrapping_mul(31) ^ (off & 511).wrapping_mul(7) ^ (off >> 17)) as u8
}

pub fn pattern_block(lba: u64, bs: usize) -> Vec<u8> {
    (0..bs as u64).map(|i| pattern(lba * bs as u64 + i)).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Cbw,
    Data,
    Csw,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The data phase of the next data command stalls after half its bytes; the CSW passes with that residue.
    StallData,
    /// The next CSW read stalls once.
    StallCsw,
    /// A zero-length packet comes before the next CSW.
    ZeroLenCsw,
    /// The next IN command sends its CSW in place of the data (skipped data phase).
    CswInData,
    /// The next CSW says phase error.
    PhaseError,
    /// The next CSW carries another tag.
    WrongTag,
    /// The next CSW of a passing command reports this residue although all data moved.
    BogusResidue(u32),
    /// The next transfer of this stage never ends (timeout); the device stays stuck until reset recovery.
    Timeout(Stage),
    /// The device is unplugged when this stage of the next command starts.
    GoneAt(Stage),
    /// The next media command fails with this sense.
    Check(Sense),
    /// The next CBW is answered with a STALL (and both pipes stay halted until reset recovery).
    StallCbw,
}

#[derive(Clone)]
pub struct Lun {
    pub block_size: usize,
    pub blocks: u64,
    pub removable: bool,
    pub write_protected: bool,
    pub dpofua: bool,
    pub medium: bool,
    pub device_type: u8,
    pub version: u8,
    pub lbpme: bool,
    pub unmap: Option<(u32, u32)>,
    pub sync_supported: bool,
    /// TEST UNIT READY says "becoming ready" this many more times.
    pub becoming_ready: u32,
    /// UNIT ATTENTIONs waiting for the next media commands.
    pub pending: VecDeque<Sense>,
    pub written: BTreeMap<u64, Vec<u8>>,
    sense: Sense,
}

impl Lun {
    pub fn new(block_size: usize, blocks: u64) -> Lun {
        Lun {
            block_size,
            blocks,
            removable: true,
            write_protected: false,
            dpofua: false,
            medium: true,
            device_type: 0,
            version: 4,
            lbpme: false,
            unmap: None,
            sync_supported: true,
            becoming_ready: 0,
            // A fresh stick: UNIT ATTENTION 29h "power on or reset" on its first media command.
            pending: VecDeque::from([Sense { key: 6, asc: 0x29, ascq: 0 }]),
            written: BTreeMap::new(),
            sense: Sense::default(),
        }
    }
    pub fn block(&self, lba: u64) -> Vec<u8> {
        self.written.get(&lba).cloned().unwrap_or_else(|| pattern_block(lba, self.block_size))
    }
}

enum Phase {
    Cbw,
    DataIn { data: Vec<u8>, pos: usize, want: usize, csw: [u8; 13] },
    DataOut { expect: usize, got: Vec<u8>, tag: u32, lun: u8, cdb: [u8; 16] },
    Csw([u8; 13]),
}

pub struct State {
    pub luns: Vec<Lun>,
    phase: Phase,
    pub halted_in: bool,
    pub halted_out: bool,
    /// An invalid CBW: both pipes stall until reset recovery (BOT 6.6.1).
    pub need_reset: bool,
    pub now: u64,
    pub gone: bool,
    pub faults: VecDeque<Fault>,
    /// What happened, in order ("cbw 28 lun 0 len 4096", "clear in", "reset", ...).
    pub log: Vec<String>,
    /// Commands moving more bytes than this make the device fail (the 090C:1000 behaviour V1 found).
    pub max_xfer: Option<usize>,
    /// Every READ moves at most this many bytes and passes with the rest as residue (a bridge that is short on
    /// every command).
    pub short_reads: Option<usize>,
    pub max_lun: Result<u8, UsbError>,
    pub reset_failures_left: u32,
    pub runs: u32,
    pub cdbs: Vec<[u8; 16]>,
    pub alloc_limit: Option<usize>,
    /// `take_poisoned` answers true once.
    pub poisoned: bool,
    /// Buffers above 64 bytes handed out.
    pub allocs: u32,
}

pub struct SimDev {
    pub st: Mutex<State>,
}

#[derive(Clone)]
pub struct SimLink(pub Arc<SimDev>);

impl SimDev {
    pub fn new(luns: Vec<Lun>) -> Arc<SimDev> {
        let max = luns.len().saturating_sub(1) as u8;
        Arc::new(SimDev {
            st: Mutex::new(State {
                luns,
                phase: Phase::Cbw,
                halted_in: false,
                halted_out: false,
                need_reset: false,
                now: 1_000_000_000,
                gone: false,
                faults: VecDeque::new(),
                log: Vec::new(),
                max_xfer: None,
                short_reads: None,
                max_lun: Ok(max),
                reset_failures_left: 0,
                runs: 0,
                cdbs: Vec::new(),
                alloc_limit: None,
                poisoned: false,
                allocs: 0,
            }),
        })
    }
    pub fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.st.lock().unwrap())
    }
    pub fn fault(&self, f: Fault) {
        self.with(|s| s.faults.push_back(f));
    }
    pub fn log_has(&self, what: &str) -> usize {
        self.with(|s| s.log.iter().filter(|l| l.starts_with(what)).count())
    }
    pub fn opcodes(&self) -> Vec<u8> {
        self.with(|s| s.cdbs.iter().map(|c| c[0]).collect())
    }
}

fn csw(tag: u32, residue: u32, status: u8) -> [u8; 13] {
    let mut b = [0u8; 13];
    b[0..4].copy_from_slice(b"USBS");
    b[4..8].copy_from_slice(&tag.to_le_bytes());
    b[8..12].copy_from_slice(&residue.to_le_bytes());
    b[12] = status;
    b
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn be64(b: &[u8]) -> u64 {
    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

fn take_fault(s: &mut State, pred: impl Fn(&Fault) -> bool) -> Option<Fault> {
    let i = s.faults.iter().position(pred)?;
    s.faults.remove(i)
}

/// The SCSI answer to a command: data-in bytes, or data-out expected, or a check condition.
#[allow(dead_code)]
enum Answer {
    In(Vec<u8>),
    Out(usize),
    None,
    Check(Sense),
}

fn sense_bytes(s: Sense) -> Vec<u8> {
    let mut b = vec![0u8; 18];
    b[0] = 0x70;
    b[2] = s.key;
    b[7] = 10;
    b[12] = s.asc;
    b[13] = s.ascq;
    b
}

const ILLEGAL_OPCODE: Sense = Sense { key: 5, asc: 0x20, ascq: 0 };
const LBA_RANGE: Sense = Sense { key: 5, asc: 0x21, ascq: 0 };

impl State {
    fn execute(&mut self, lun: u8, cdb: &[u8; 16], len: usize) -> Answer {
        self.cdbs.push(*cdb);
        let opc = cdb[0];
        let Some(l) = self.luns.get_mut(lun as usize) else {
            return Answer::Check(Sense { key: 5, asc: 0x25, ascq: 0 });
        };
        if opc == op::REQUEST_SENSE {
            let s = core::mem::take(&mut l.sense);
            return Answer::In(sense_bytes(s));
        }
        if opc == op::INQUIRY {
            if cdb[1] & 1 != 0 {
                let unmap = l.unmap;
                return match (cdb[2], unmap) {
                    (0x00, _) if l.version >= 5 => Answer::In(vec![0, 0, 0, 4, 0x00, 0x80, 0xB0, 0xB2]),
                    (0xB2, Some(_)) => Answer::In(vec![0, 0xB2, 0, 4, 0, 0x80, 0, 0]),
                    (0xB0, Some((lba, desc))) => {
                        let mut b = vec![0u8; 64];
                        b[1] = 0xB0;
                        b[3] = 0x3C;
                        b[20..24].copy_from_slice(&lba.to_be_bytes());
                        b[24..28].copy_from_slice(&desc.to_be_bytes());
                        Answer::In(b)
                    }
                    _ => Answer::Check(Sense { key: 5, asc: 0x24, ascq: 0 }),
                };
            }
            let mut b = vec![0u8; 36];
            b[0] = l.device_type;
            b[1] = if l.removable { 0x80 } else { 0 };
            b[2] = l.version;
            b[4] = 31;
            b[8..16].copy_from_slice(b"BITSSIM ");
            b[16..32].copy_from_slice(b"SIMULATED STICK ");
            b[32..36].copy_from_slice(b"1.00");
            return Answer::In(b);
        }
        // Media commands from here on.
        if let Some(s) = l.pending.pop_front() {
            l.sense = s;
            return Answer::Check(s);
        }
        if let Some(Fault::Check(s)) = take_fault(self, |f| matches!(f, Fault::Check(_))) {
            self.luns[lun as usize].sense = s;
            return Answer::Check(s);
        }
        let l = &mut self.luns[lun as usize];
        let no_medium = Sense { key: 2, asc: 0x3A, ascq: 0 };
        if !l.medium && opc != op::MODE_SENSE_6 {
            l.sense = no_medium;
            return Answer::Check(no_medium);
        }
        let bs = l.block_size;
        match opc {
            op::TEST_UNIT_READY => {
                if l.becoming_ready > 0 {
                    l.becoming_ready -= 1;
                    l.sense = Sense { key: 2, asc: 0x04, ascq: 0x01 };
                    return Answer::Check(l.sense);
                }
                Answer::None
            }
            op::READ_CAPACITY_10 => {
                let last = if l.blocks > 0xFFFF_FFFF { 0xFFFF_FFFF } else { (l.blocks - 1) as u32 };
                let mut b = last.to_be_bytes().to_vec();
                b.extend_from_slice(&(bs as u32).to_be_bytes());
                Answer::In(b)
            }
            op::SERVICE_ACTION_IN_16 if cdb[1] & 0x1F == op::SAI_READ_CAPACITY_16 => {
                if l.version < 5 && l.blocks <= 0xFFFF_FFFF {
                    l.sense = ILLEGAL_OPCODE;
                    return Answer::Check(ILLEGAL_OPCODE);
                }
                let mut b = vec![0u8; 32];
                b[0..8].copy_from_slice(&(l.blocks - 1).to_be_bytes());
                b[8..12].copy_from_slice(&(bs as u32).to_be_bytes());
                b[14] = if l.lbpme { 0x80 } else { 0 };
                Answer::In(b)
            }
            op::MODE_SENSE_6 => {
                let d = if l.write_protected { 0x80 } else { 0 } | if l.dpofua { 0x10 } else { 0 };
                Answer::In(vec![3, 0, d, 0])
            }
            op::READ_10 | op::READ_16 | op::WRITE_10 | op::WRITE_16 => {
                let (lba, n) = if opc == op::READ_10 || opc == op::WRITE_10 {
                    (be32(&cdb[2..6]) as u64, u16::from_be_bytes([cdb[7], cdb[8]]) as u64)
                } else {
                    (be64(&cdb[2..10]), be32(&cdb[10..14]) as u64)
                };
                if lba.checked_add(n).is_none_or(|e| e > l.blocks) {
                    l.sense = LBA_RANGE;
                    return Answer::Check(LBA_RANGE);
                }
                let bytes = n as usize * bs;
                if self.max_xfer.is_some_and(|m| bytes > m) {
                    // What a too-big command does to such a stick: it fails it.
                    let l = &mut self.luns[lun as usize];
                    l.sense = Sense { key: 4, asc: 0x44, ascq: 0 };
                    self.log.push(alloc::format!("too big {bytes}"));
                    return Answer::Check(Sense { key: 4, asc: 0x44, ascq: 0 });
                }
                if opc == op::READ_10 || opc == op::READ_16 {
                    let mut v = Vec::with_capacity(bytes);
                    for b in lba..lba + n {
                        v.extend_from_slice(&l.block(b));
                    }
                    if let Some(m) = self.short_reads {
                        v.truncate(m);
                    }
                    let _ = len;
                    Answer::In(v)
                } else if l.write_protected {
                    l.sense = Sense { key: 7, asc: 0x27, ascq: 0 };
                    Answer::Check(l.sense)
                } else {
                    Answer::Out(bytes)
                }
            }
            op::SYNCHRONIZE_CACHE_10 => {
                if l.sync_supported {
                    Answer::None
                } else {
                    l.sense = ILLEGAL_OPCODE;
                    Answer::Check(ILLEGAL_OPCODE)
                }
            }
            op::UNMAP if l.unmap.is_some() => Answer::Out(u16::from_be_bytes([cdb[7], cdb[8]]) as usize),
            _ => {
                l.sense = ILLEGAL_OPCODE;
                Answer::Check(ILLEGAL_OPCODE)
            }
        }
    }

    fn write_done(&mut self, lun: u8, cdb: &[u8; 16], data: &[u8]) {
        let l = &mut self.luns[lun as usize];
        let bs = l.block_size;
        match cdb[0] {
            op::WRITE_10 | op::WRITE_16 => {
                let lba = if cdb[0] == op::WRITE_10 { be32(&cdb[2..6]) as u64 } else { be64(&cdb[2..10]) };
                for (i, chunk) in data.chunks(bs).enumerate() {
                    l.written.insert(lba + i as u64, chunk.to_vec());
                }
                self.log.push(alloc::format!("write {lba} {} fua {}", data.len() / bs, cdb[1] & 8 != 0));
            }
            op::UNMAP => {
                let n = (u16::from_be_bytes([data[2], data[3]]) as usize) / 16;
                for k in 0..n {
                    let d = &data[8 + k * 16..8 + k * 16 + 16];
                    self.log.push(alloc::format!("unmap {} {}", be64(&d[0..8]), be32(&d[8..12])));
                }
            }
            _ => {}
        }
    }

    fn cbw(&mut self, b: &[u8]) -> XferResult {
        if let Some(Fault::GoneAt(Stage::Cbw)) = take_fault(self, |f| *f == Fault::GoneAt(Stage::Cbw)) {
            self.gone = true;
            return XferResult::failed(UsbError::Gone, 0);
        }
        if take_fault(self, |f| *f == Fault::Timeout(Stage::Cbw)).is_some() {
            self.now += CBW_TIMEOUT_NS;
            return XferResult::failed(UsbError::Timeout, 0);
        }
        if take_fault(self, |f| *f == Fault::StallCbw).is_some() || b.len() != 31 || &b[0..4] != b"USBC" {
            self.need_reset = true;
            self.halted_in = true;
            self.halted_out = true;
            self.log.push("invalid cbw".into());
            return XferResult::failed(UsbError::Stall, 0);
        }
        let tag = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
        let len = u32::from_le_bytes([b[8], b[9], b[10], b[11]]) as usize;
        let dir_in = b[12] & 0x80 != 0;
        let lun = b[13] & 0x0F;
        let mut cdb = [0u8; 16];
        cdb.copy_from_slice(&b[15..31]);
        self.log.push(alloc::format!("cbw {:02x} lun {lun} len {len}", cdb[0]));
        let answer = self.execute(lun, &cdb, len);
        let tag_out = if take_fault(self, |f| *f == Fault::WrongTag).is_some() { tag.wrapping_add(1) } else { tag };
        let status_ok = if take_fault(self, |f| *f == Fault::PhaseError).is_some() { 2 } else { 0 };
        self.phase = match answer {
            Answer::In(mut data) if dir_in && len > 0 => {
                data.truncate(len);
                let residue = (len - data.len()) as u32;
                let bogus = match take_fault(self, |f| matches!(f, Fault::BogusResidue(_))) {
                    Some(Fault::BogusResidue(r)) => r,
                    _ => residue,
                };
                if take_fault(self, |f| *f == Fault::CswInData).is_some() {
                    Phase::DataIn { data: csw(tag_out, len as u32, status_ok).to_vec(), pos: 0, want: len, csw: [0; 13] }
                } else {
                    Phase::DataIn { want: len, csw: csw(tag_out, bogus, status_ok), data, pos: 0 }
                }
            }
            Answer::In(_) | Answer::None if len == 0 => Phase::Csw(csw(tag_out, 0, status_ok)),
            Answer::Out(n) if !dir_in && len > 0 => Phase::DataOut { expect: n.min(len), got: Vec::new(), tag: tag_out, lun, cdb },
            Answer::Check(_) => {
                // Failed command: an IN data phase stalls, an OUT one too (BOT 6.7.2/6.7.3), CSW status 1.
                if len > 0 {
                    if dir_in {
                        self.halted_in = true;
                    } else {
                        self.halted_out = true;
                    }
                }
                Phase::Csw(csw(tag_out, len as u32, 1))
            }
            // Direction or length disagree with the command: phase error.
            _ => Phase::Csw(csw(tag_out, len as u32, 2)),
        };
        XferResult::ok(31)
    }

    fn bulk_out(&mut self, data: &[u8]) -> XferResult {
        if self.gone {
            return XferResult::failed(UsbError::Gone, 0);
        }
        if self.halted_out {
            return XferResult::failed(UsbError::Stall, 0);
        }
        match core::mem::replace(&mut self.phase, Phase::Cbw) {
            Phase::Cbw => self.cbw(data),
            Phase::DataOut { expect, mut got, tag, lun, cdb } => {
                if take_fault(self, |f| *f == Fault::GoneAt(Stage::Data)).is_some() {
                    self.gone = true;
                    return XferResult::failed(UsbError::Gone, 0);
                }
                let k = data.len().min(expect - got.len());
                got.extend_from_slice(&data[..k]);
                if got.len() >= expect {
                    self.write_done(lun, &cdb, &got);
                    self.phase = Phase::Csw(csw(tag, 0, 0));
                } else {
                    self.phase = Phase::DataOut { expect, got, tag, lun, cdb };
                }
                XferResult::ok(k)
            }
            other => {
                self.phase = other;
                self.halted_out = true;
                XferResult::failed(UsbError::Stall, 0)
            }
        }
    }

    fn bulk_in(&mut self, len: usize) -> (XferResult, Vec<u8>) {
        if self.gone {
            return (XferResult::failed(UsbError::Gone, 0), Vec::new());
        }
        if self.halted_in {
            return (XferResult::failed(UsbError::Stall, 0), Vec::new());
        }
        match core::mem::replace(&mut self.phase, Phase::Cbw) {
            Phase::DataIn { data, pos, want, csw } => {
                if take_fault(self, |f| *f == Fault::GoneAt(Stage::Data)).is_some() {
                    self.gone = true;
                    return (XferResult::failed(UsbError::Gone, 0), Vec::new());
                }
                if take_fault(self, |f| *f == Fault::Timeout(Stage::Data)).is_some() {
                    self.now += crate::link::data_timeout_ns(len);
                    self.phase = Phase::DataIn { data, pos, want, csw };
                    return (XferResult::failed(UsbError::Timeout, 0), Vec::new());
                }
                if take_fault(self, |f| *f == Fault::StallData).is_some() {
                    let half = (data.len() - pos).min(len) / 2;
                    let out = data[pos..pos + half].to_vec();
                    self.halted_in = true;
                    let mut c = csw;
                    let residue = (want - pos - half) as u32;
                    c[8..12].copy_from_slice(&residue.to_le_bytes());
                    self.phase = Phase::Csw(c);
                    return (XferResult::failed(UsbError::Stall, half), out);
                }
                let k = (data.len() - pos).min(len);
                let out = data[pos..pos + k].to_vec();
                if pos + k >= data.len() {
                    // All the device has (possibly less than asked: short packet).
                    if csw == [0; 13] {
                        self.phase = Phase::Cbw; // the CSW went in place of the data
                    } else {
                        self.phase = Phase::Csw(csw);
                    }
                } else {
                    self.phase = Phase::DataIn { data, pos: pos + k, want, csw };
                }
                (XferResult::ok(k), out)
            }
            Phase::Csw(c) => {
                if take_fault(self, |f| *f == Fault::GoneAt(Stage::Csw)).is_some() {
                    self.gone = true;
                    return (XferResult::failed(UsbError::Gone, 0), Vec::new());
                }
                if take_fault(self, |f| *f == Fault::Timeout(Stage::Csw)).is_some() {
                    self.now += CSW_TIMEOUT_NS;
                    self.phase = Phase::Csw(c);
                    return (XferResult::failed(UsbError::Timeout, 0), Vec::new());
                }
                if take_fault(self, |f| *f == Fault::ZeroLenCsw).is_some() {
                    self.phase = Phase::Csw(c);
                    return (XferResult::ok(0), Vec::new());
                }
                if take_fault(self, |f| *f == Fault::StallCsw).is_some() {
                    self.phase = Phase::Csw(c);
                    self.halted_in = true;
                    return (XferResult::failed(UsbError::Stall, 0), Vec::new());
                }
                let k = len.min(13);
                (XferResult::ok(k), c[..k].to_vec())
            }
            other => {
                // Nothing to send: the device NAKs until the host gives up.
                self.phase = other;
                self.now += CSW_TIMEOUT_NS;
                (XferResult::failed(UsbError::Timeout, 0), Vec::new())
            }
        }
    }
}

impl Link for SimLink {
    fn name(&self) -> String {
        "sim0-1 if0".into()
    }
    fn alloc(&self, len: usize) -> Result<Buffer, UsbError> {
        if self.0.with(|s| {
            // Data buffers only (the default `run` takes a small CBW/CSW buffer each time).
            s.allocs += (len > 64) as u32;
            s.alloc_limit.is_some_and(|m| len > m)
        }) {
            return Err(UsbError::NoMemory);
        }
        Ok(Arc::new(Mem(Mutex::new(vec![0u8; len]))))
    }
    fn bulk(&self, dir_in: bool, req: Request, _timeout_ns: u64) -> XferResult {
        let mut s = self.0.st.lock().unwrap();
        if dir_in {
            let (r, bytes) = s.bulk_in(req.len);
            if !req.buf.write(req.offset, &bytes) {
                return XferResult::failed(UsbError::Invalid("range"), 0);
            }
            r
        } else {
            let mut v = vec![0u8; req.len];
            if !req.buf.read(req.offset, &mut v) {
                return XferResult::failed(UsbError::Invalid("range"), 0);
            }
            s.bulk_out(&v)
        }
    }
    fn clear_halt(&self, dir_in: bool) -> Result<(), UsbError> {
        let mut s = self.0.st.lock().unwrap();
        if s.gone {
            return Err(UsbError::Gone);
        }
        s.log.push(if dir_in { "clear in".into() } else { "clear out".into() });
        if !s.need_reset {
            if dir_in {
                s.halted_in = false;
            } else {
                s.halted_out = false;
            }
        }
        Ok(())
    }
    fn abort_all(&self) {
        self.0.with(|s| s.log.push("abort".into()));
    }
    fn bot_reset(&self) -> Result<(), UsbError> {
        let mut s = self.0.st.lock().unwrap();
        if s.gone {
            return Err(UsbError::Gone);
        }
        if s.reset_failures_left > 0 {
            s.reset_failures_left -= 1;
            s.log.push("reset failed".into());
            return Err(UsbError::Stall);
        }
        s.log.push("reset".into());
        s.need_reset = false;
        s.phase = Phase::Cbw;
        Ok(())
    }
    fn max_lun(&self) -> Result<u8, UsbError> {
        self.0.with(|s| if s.gone { Err(UsbError::Gone) } else { s.max_lun })
    }
    fn gone(&self) -> bool {
        self.0.with(|s| s.gone)
    }
    fn now_ns(&self) -> u64 {
        self.0.with(|s| s.now)
    }
    fn sleep_ns(&self, ns: u64) {
        self.0.with(|s| s.now += ns);
    }
    fn take_poisoned(&self) -> bool {
        self.0.with(|s| core::mem::take(&mut s.poisoned))
    }
    fn run(&self, cmds: &[crate::link::Cmd]) -> Vec<crate::link::Ran> {
        self.0.with(|s| s.runs += 1);
        // The default implementation, through `bulk`.
        DefaultRun(self).run(cmds)
    }
}

/// Calls the trait's default `run` for a link that overrides it.
struct DefaultRun<'a>(&'a SimLink);

impl Link for DefaultRun<'_> {
    fn name(&self) -> String {
        self.0.name()
    }
    fn alloc(&self, len: usize) -> Result<Buffer, UsbError> {
        self.0.alloc(len)
    }
    fn bulk(&self, dir_in: bool, req: Request, t: u64) -> XferResult {
        self.0.bulk(dir_in, req, t)
    }
    fn clear_halt(&self, dir_in: bool) -> Result<(), UsbError> {
        self.0.clear_halt(dir_in)
    }
    fn abort_all(&self) {
        self.0.abort_all()
    }
    fn bot_reset(&self) -> Result<(), UsbError> {
        self.0.bot_reset()
    }
    fn max_lun(&self) -> Result<u8, UsbError> {
        self.0.max_lun()
    }
    fn gone(&self) -> bool {
        self.0.gone()
    }
    fn now_ns(&self) -> u64 {
        self.0.now_ns()
    }
    fn sleep_ns(&self, ns: u64) {
        self.0.sleep_ns(ns)
    }
}
