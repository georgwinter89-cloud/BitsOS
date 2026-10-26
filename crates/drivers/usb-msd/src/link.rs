//! What the Bulk-Only layer needs from a device: its two bulk endpoints, endpoint 0 for the two class requests,
//! DMA buffers and a clock. `usblink::UsbLink` is the real one (US-3's transfer API, with commands chained from
//! completion callbacks: V1's K-2 queued reads), the host tests use a simulated stick (`sim`).

use alloc::vec::Vec;
use usb_core::{Buffer, Request, UsbError, XferResult};

/// Direction of a command's data phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    None,
    In,
    Out,
}

/// One Bulk-Only command ready to go: its 31-byte CBW and where its data goes or comes from.
#[derive(Clone)]
pub struct Cmd {
    pub cbw: [u8; 31],
    pub dir: Dir,
    /// The data phase (`dir` != None and a length above 0), in a buffer of this link.
    pub data: Option<Request>,
    /// How long the data phase may take (V1: 10 s per 64 KiB).
    pub data_timeout_ns: u64,
    /// The device's residues are ignored (`bot::clean` then accepts a CSW with a residue).
    pub residue_ignored: bool,
}

impl Cmd {
    pub fn data_len(&self) -> usize {
        self.data.as_ref().map_or(0, |r| r.len)
    }
}

/// What happened to one command's stages as far as they ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ran {
    pub cbw: XferResult,
    /// The data phase, if it was started.
    pub data: Option<XferResult>,
    /// The status phase and its bytes, if it was read.
    pub csw: Option<(XferResult, [u8; 13])>,
}

impl Ran {
    pub fn cbw_only(cbw: XferResult) -> Ran {
        Ran { cbw, data: None, csw: None }
    }
}

/// Timeouts of one command's stages (V1 `usb/msd.asm:38-40`: CBW 5 s, data 10 s per 64 KiB, CSW 20 s because
/// cheap sticks program their flash before the status; Linux waits 30 s for the whole command).
pub const CBW_TIMEOUT_NS: u64 = 5_000_000_000;
pub const CSW_TIMEOUT_NS: u64 = 20_000_000_000;
pub const DATA_TIMEOUT_PER_64K_NS: u64 = 10_000_000_000;

pub fn data_timeout_ns(len: usize) -> u64 {
    (len.div_ceil(65536).max(1) as u64).saturating_mul(DATA_TIMEOUT_PER_64K_NS)
}

pub trait Link: Send + Sync {
    /// "xhci0-1.3 if0", for logs.
    fn name(&self) -> alloc::string::String;
    /// DMA memory for data phases, zeroed.
    fn alloc(&self, len: usize) -> Result<Buffer, UsbError>;
    /// One transfer on the bulk IN (`dir_in`) or OUT endpoint; ends at the latest after `timeout_ns` (cancelled).
    fn bulk(&self, dir_in: bool, req: Request, timeout_ns: u64) -> XferResult;
    /// CLEAR_FEATURE(ENDPOINT_HALT) and the host side of the endpoint back to toggle 0.
    fn clear_halt(&self, dir_in: bool) -> Result<(), UsbError>;
    /// Cancels whatever is still queued on both bulk endpoints (after a timeout, before a reset).
    fn abort_all(&self);
    /// Bulk-Only Mass Storage Reset (class request FFh to the interface).
    fn bot_reset(&self) -> Result<(), UsbError>;
    /// GET MAX LUN (class request FEh): the highest LUN.
    fn max_lun(&self) -> Result<u8, UsbError>;
    /// The device was detached.
    fn gone(&self) -> bool;
    fn now_ns(&self) -> u64;
    fn sleep_ns(&self, ns: u64);
    /// True once after a run was given up with transfers the controller never returned: buffers used so far
    /// may still be held by the controller (US-3 refuses them for new transfers), so cached ones are dropped.
    fn take_poisoned(&self) -> bool {
        false
    }

    /// Runs `cmds` one after the other (CBW, data, CSW each) and stops after the first command that is not clean
    /// (`bot::clean`: every stage whole and without error, the CSW passed with residue 0): its stages ran as far
    /// as they got without error, a data phase that failed or came short ends it before the CSW. The result has
    /// one entry per command started. The default does it with single transfers; `UsbLink` chains every next
    /// stage from the previous one's completion callback, so a whole run costs one wake-up.
    fn run(&self, cmds: &[Cmd]) -> Vec<Ran> {
        let mut out = Vec::new();
        let small = match self.alloc(64) {
            Ok(b) => b,
            Err(e) => {
                if !cmds.is_empty() {
                    out.push(Ran::cbw_only(XferResult::failed(e, 0)));
                }
                return out;
            }
        };
        for c in cmds {
            if !small.write(0, &c.cbw) {
                out.push(Ran::cbw_only(XferResult::failed(UsbError::NoMemory, 0)));
                break;
            }
            let cbw = self.bulk(false, Request::new(small.clone(), 0, 31), CBW_TIMEOUT_NS);
            let mut ran = Ran::cbw_only(cbw);
            if cbw.error.is_some() || cbw.actual != 31 {
                out.push(ran);
                break;
            }
            if let Some(req) = c.data.clone() {
                let len = req.len;
                let r = self.bulk(c.dir == Dir::In, req, c.data_timeout_ns);
                ran.data = Some(r);
                if r.error.is_some() || r.actual != len {
                    out.push(ran);
                    break;
                }
            }
            let r = self.bulk(true, Request::new(small.clone(), 32, 13), CSW_TIMEOUT_NS);
            let mut csw = [0u8; 13];
            let _ = small.read(32, &mut csw[..r.actual.min(13)]);
            ran.csw = Some((r, csw));
            out.push(ran);
            if !crate::bot::clean(c, &ran) {
                break;
            }
        }
        out
    }
}
