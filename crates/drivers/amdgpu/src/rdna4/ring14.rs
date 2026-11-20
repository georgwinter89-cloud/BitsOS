//! GP-GPU2: the card's KM ring and its command protocol, as `psp_load_fw` runs it for PSP 14.0.3 (amdgpu_psp.c:
//! 3595-3660): the buffers (`psp_ring_init` :66, `psp_sw_init` :539-560), ring create on bare metal
//! (psp_v14_0.c:298-336), one 1024-byte command per frame with a fence (`psp_cmd_submit_buf` :726-799,
//! `psp_ring_cmd_submit` :3917-3960), ring destroy (psp_v14_0.c:239-268, :340-356).
//!
//! The frame and the write-pointer arithmetic are `gpucore::psp`'s (the 780M's, version-independent). The register
//! half is this card's own (`Mailbox14`), in Linux's form to the access: the waits are Linux's loop (at most 100000
//! reads 1 us apart for 0x80000000 under 0x8000FFFF, no clock), the 20 ms sleep after a ring control, and no read
//! Linux does not make, so a stage's trail equals the register trace of Linux's own start (`linuxseq`, section
//! psp-ring). The 780M's `Mailbox13` waits by the clock and for bit 31 alone and stays as it is.
//!
//! The sequence differs from 13.0.4's: the firmware keeps a boot-time TMR (no SETUP_TMR, no DESTROY_TMR,
//! amdgpu_psp.c:184-185, :917, :938) and the secure OS reports its reserved region first
//! (`psp_update_fw_reservation` :1065-1120). Stricter than Linux: a non-zero command status is an error unless the
//! caller takes it (`submit_any`: Linux only warns, :763-799); the reserved-region questions tolerate "unknown
//! command" as Linux does (:1047-1052).

use super::vram::{Bump, Layout};
use crate::gpucore::psp::{self, BoAlloc, Buf, Cond, Hw, KmRing, Mailbox, PspError, Timeouts, encode_arr};
use crate::ipmod::psp13::Addrs;

/// `MBOX_TOS_READY_FLAG` under `MBOX_TOS_READY_MASK`, and the same for the response (amdgpu_psp.h:57-58, :64-65):
/// bit 31 set and the status field 0.
pub const ANSWERED: Cond = Cond::Masked { mask: psp::FLAG_RESPONSE | psp::STATUS_MASK, value: psp::FLAG_RESPONSE };
/// `mdelay(20)` after a ring control (psp_v14_0.c:259-260, :328-329).
pub const SETTLE_US: u64 = 20_000;

/// Linux's wait never ends on an answer that carries a status: it runs out. Said by name, so the stop tells the
/// status instead of a timeout.
fn answered(r: Result<u32, PspError>) -> Result<u32, PspError> {
    match r {
        Err(PspError::Timeout { last, .. }) if last & psp::FLAG_RESPONSE != 0 && last & psp::STATUS_MASK != 0 => Err(PspError::RingStatus { status: (last & psp::STATUS_MASK) as u16 }),
        other => other,
    }
}

/// The KM ring's register half for PSP 14.0.3 on bare metal, access for access as psp_v14_0.c does it.
#[derive(Clone, Copy, Debug)]
pub struct Mailbox14 {
    pub addrs: Addrs,
    /// The next frame is the first of a ring just created: its write pointer must read 0. Linux does not read the
    /// pointer after create; the first frame's own read (`psp_ring_cmd_submit`, amdgpu_psp.c:3927) is the check.
    pub first: bool,
}

impl Mailbox14 {
    /// psp_v14_0.c:302-309: the secure OS ready for a ring.
    pub fn ready(&self, hw: &mut dyn Hw) -> Result<u32, PspError> {
        answered(psp::wait_reads(hw, self.addrs.c2pmsg_64, ANSWERED, psp::LINUX_POLLS, "ring.ready"))
    }

    /// psp_v14_0.c:311-331: address, size, the command, 20 ms, the answer.
    pub fn send_create(&self, hw: &mut dyn Hw, mc: u64, size: u32) -> Result<(), PspError> {
        let a = self.addrs;
        psp::write(hw, a.c2pmsg_69, mc as u32)?;
        psp::write(hw, a.c2pmsg_70, (mc >> 32) as u32)?;
        psp::write(hw, a.c2pmsg_71, size)?;
        psp::write(hw, a.c2pmsg_64, psp::RING_TYPE_KM << 16)?;
        hw.delay_us(SETTLE_US);
        answered(psp::wait_reads(hw, a.c2pmsg_64, ANSWERED, psp::LINUX_POLLS, "ring.create")).map(|_| ())
    }
}

impl Mailbox for Mailbox14 {
    fn sos(&self, hw: &mut dyn Hw) -> Result<u32, PspError> {
        psp::read(hw, self.addrs.c2pmsg_81)
    }

    fn ring_create(&self, hw: &mut dyn Hw, _t: &Timeouts, mc: u64, size: u32) -> Result<(), PspError> {
        self.ready(hw)?;
        self.send_create(hw, mc, size)
    }

    /// psp_v14_0.c:254-266: DESTROY_RINGS, 20 ms, the answer (a status means the ring is still up).
    fn ring_stop(&self, hw: &mut dyn Hw, _t: &Timeouts) -> Result<(), PspError> {
        psp::write(hw, self.addrs.c2pmsg_64, psp::CTRL_DESTROY_RINGS)?;
        hw.delay_us(SETTLE_US);
        answered(psp::wait_reads(hw, self.addrs.c2pmsg_64, ANSWERED, psp::LINUX_POLLS, "ring.stop")).map(|_| ())
    }

    fn wptr(&self, hw: &mut dyn Hw) -> Result<u32, PspError> {
        match psp::read(hw, self.addrs.c2pmsg_67)? {
            // A frame at another place than the ring's start would make the secure OS run an old ring's frames
            // before it (or skip ours): nothing is written.
            w if self.first && w != 0 => Err(PspError::RingNotReset { wptr: w }),
            w => Ok(w),
        }
    }

    fn set_wptr(&self, hw: &mut dyn Hw, dwords: u32) -> Result<(), PspError> {
        psp::write(hw, self.addrs.c2pmsg_67, dwords)
    }

    fn ring_size_reg(&self, hw: &mut dyn Hw) -> Result<u32, PspError> {
        psp::read(hw, self.addrs.c2pmsg_71)
    }
}

/// psp_gfx_if.h:111-112.
pub const FB_FW_RESERV_ADDR: u32 = 0x50;
pub const FB_FW_RESERV_EXT_ADDR: u32 = 0x51;
/// `psp_gfx_resp.uresp` at +64 of the response (psp_gfx_if.h:448-461): reserve_base_address_hi, _lo, reserve_size
/// (:422-426).
pub const URESP_AT: usize = psp::RESP_AT + 64;

/// One answer: the common response and the union's first three dwords.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Answer {
    pub resp: psp::Resp,
    pub uresp: [u32; 3],
}

/// A reserved region as the secure OS names it: base (VRAM offset) and size; both 0 when it does not know the
/// question.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reserved {
    pub base: u64,
    pub size: u32,
}

impl Reserved {
    fn of(a: &Answer) -> Reserved {
        if a.resp.status == psp::PSP_ERR_UNKNOWN_COMMAND {
            return Reserved::default();
        }
        Reserved { base: u64::from(a.uresp[0]) << 32 | u64::from(a.uresp[1]), size: a.uresp[2] }
    }
}

pub struct Ring14 {
    pub addrs: Addrs,
    /// The fence's bound (Linux: 20000 polls of 60 to 100 us, amdgpu_psp.c:292, :751-764).
    pub t: Timeouts,
    pub ring: KmRing,
    pub cmd: Buf,
    pub fence: Buf,
    fence_value: u32,
    wedged: bool,
    created: bool,
    /// The ring's registers may have reached the card (create got past its ready check): from then on only a
    /// destroy says the ring is gone, whatever create answered.
    sent: bool,
    /// The write pointer as this ring last wrote it (dwords), for the note: Linux does not read it back.
    wptr: u32,
}

fn buf(bo: &mut Bump, l: &Layout, size: u64, align: u64) -> Result<Buf, PspError> {
    let (handle, r) = bo.alloc(size, align).map_err(PspError::Bo)?;
    Ok(Buf { handle, offset: r.start, size: r.end - r.start, mc: l.mc(r.start) })
}

/// A `static`, not a `const`: a use of a constant this large may put a copy of it on the stack of the thread a
/// stage runs on (MCP's 64 KiB).
static ZEROS: [u8; 0x1_0000] = [0; 0x1_0000];

fn zero(hw: &mut dyn Hw, mc: u64, bytes: u64) -> Result<(), PspError> {
    let mut at = 0;
    while at < bytes {
        let n = (bytes - at).min(ZEROS.len() as u64);
        put(hw, mc + at, &ZEROS[..n as usize])?;
        at += n;
    }
    Ok(())
}

/// The 1 MiB firmware buffer (`fw_pri`, amdgpu_psp.c:539-548): what the bootloader reads a part from and what the
/// TOC, the register list and the trusted applications are handed over in. In VRAM, as every buffer of the secure
/// processor here (`vram`).
pub struct Pri {
    pub buf: Buf,
    /// How many bytes from the start may be non-zero; `None` before the first load (whatever VRAM held).
    used: Option<u64>,
}

impl Pri {
    pub fn new(bo: &mut Bump, l: &Layout) -> Result<Pri, PspError> {
        Ok(Pri { buf: buf(bo, l, psp::PRI_BUF_BYTES, psp::PRI_BUF_BYTES)?, used: None })
    }

    /// `psp_copy_fw` (amdgpu_psp.c:4657-4675): afterwards the buffer holds `data` and zeros up to its megabyte,
    /// as Linux's memset and memcpy leave it. The bootloader is told no size, so what lies behind a part counts.
    /// Only the bytes an earlier load left are zeroed again (the first load zeroes all of it).
    pub fn load(&mut self, hw: &mut dyn Hw, data: &[u8]) -> Result<(), PspError> {
        let len = data.len() as u64;
        if len == 0 || len > psp::PRI_BUF_BYTES {
            return Err(PspError::TocSize(data.len()));
        }
        let end = self.used.unwrap_or(psp::PRI_BUF_BYTES);
        // From here the buffer may hold anything until the writes are through.
        self.used = None;
        // Whole dwords only: the card's memory is written a dword at a time, and the bytes behind a part that
        // does not end on one must be zeros too, not what lay there.
        let whole = data.len() & !3;
        put(hw, self.buf.mc, &data[..whole])?;
        if whole != data.len() {
            let mut last = [0u8; 4];
            last[..data.len() - whole].copy_from_slice(&data[whole..]);
            put(hw, self.buf.mc + whole as u64, &last)?;
        }
        let from = len.next_multiple_of(4);
        if end > from {
            zero(hw, self.buf.mc + from, end - from)?;
        }
        self.used = Some(from);
        Ok(())
    }
}

impl Ring14 {
    /// The ring, fence and command pages in the window (VRAM, never GTT: `vram`). Nothing is written to the card.
    pub fn new(bo: &mut Bump, l: &Layout, addrs: Addrs, t: Timeouts) -> Result<Ring14, PspError> {
        let ring = buf(bo, l, u64::from(psp::RING_BYTES), crate::bo::PAGE)?;
        let ring = KmRing::new(ring, psp::RING_BYTES).ok_or(PspError::RingSize(psp::RING_BYTES))?;
        let fence = buf(bo, l, psp::FENCE_BUF_BYTES, crate::bo::PAGE)?;
        let cmd = buf(bo, l, psp::CMD_BUF_BYTES, crate::bo::PAGE)?;
        Ok(Ring14 { addrs, t, ring, cmd, fence, fence_value: 0, wedged: false, created: false, sent: false, wptr: 0 })
    }

    pub fn created(&self) -> bool {
        self.created
    }

    pub fn wedged(&self) -> bool {
        self.wedged
    }

    pub fn sent(&self) -> bool {
        self.sent
    }

    /// The write pointer after this ring's last frame (dwords).
    pub fn wptr(&self) -> u32 {
        self.wptr
    }

    /// `psp_load_fw` (amdgpu_psp.c:3603-3609): the fence buffer, the ring's memory and the command page cleared, then
    /// `psp_v14_0_ring_create`: Linux's ready wait (a secure OS that shows ready with a status gets nothing, as
    /// Linux stops there), the registers, the answer.
    pub fn create(&mut self, hw: &mut dyn Hw) -> Result<(), PspError> {
        if self.created || self.sent {
            return Err(PspError::Started);
        }
        zero(hw, self.fence.mc, psp::FENCE_BUF_BYTES)?;
        zero(hw, self.ring.buf.mc, u64::from(psp::RING_BYTES))?;
        // Linux clears the whole command page before each command (amdgpu_psp.c:739); only its first 1024 bytes
        // are ever written again, so once is the same.
        zero(hw, self.cmd.mc, psp::CMD_BUF_BYTES)?;
        let mb = Mailbox14 { addrs: self.addrs, first: false };
        mb.ready(hw)?;
        self.sent = true;
        mb.send_create(hw, self.ring.buf.mc, self.ring.bytes())?;
        self.created = true;
        self.ring.reset();
        (self.fence_value, self.wptr) = (0, 0);
        Ok(())
    }

    fn listed(id: u32, words: &[u32]) -> Result<(), PspError> {
        match rdna4_core::refuse::ring_command_refused(id, words) {
            Some(why) => {
                klog::event!(Error, "gx", "rdna4.psp.refused", cmd = id, why = why; "a ring command off the allow list");
                Err(PspError::CmdRefused { cmd: id, why })
            }
            None => Ok(()),
        }
    }

    /// One command, its fence awaited; the answer read back (status fatal unless `tolerate` holds it). Only the
    /// commands and image types of `rdna4_core::refuse::ring_command_refused` go out; any other is refused before
    /// the command page is written.
    pub fn submit(&mut self, hw: &mut dyn Hw, id: u32, words: &[u32], tolerate: &[u32]) -> Result<Answer, PspError> {
        Self::listed(id, words)?;
        let a = self.submit_raw(hw, id, words)?;
        if a.resp.status != 0 && !tolerate.contains(&a.resp.status) {
            klog::event!(Error, "gx", "rdna4.psp.status", cmd = id, status = a.resp.status; "non-zero status: fatal");
            return Err(PspError::CmdStatus { cmd: id, status: a.resp.status, ucode: None });
        }
        Ok(a)
    }

    /// `submit` whose caller judges the status itself, as Linux does for everything (`psp_cmd_submit_buf` only
    /// warns, amdgpu_psp.c:763-799): for the optional trusted applications, whose refusal Linux logs and passes
    /// over (`psp_load_fw` :3645-3670).
    pub fn submit_any(&mut self, hw: &mut dyn Hw, id: u32, words: &[u32]) -> Result<Answer, PspError> {
        Self::listed(id, words)?;
        self.submit_raw(hw, id, words)
    }

    /// A command without the allow list and with any status: the tests use it to prove the model refuses what
    /// the list keeps out.
    pub(crate) fn submit_raw(&mut self, hw: &mut dyn Hw, id: u32, words: &[u32]) -> Result<Answer, PspError> {
        if self.wedged {
            return Err(PspError::Wedged);
        }
        if !self.created {
            return Err(PspError::NoRing);
        }
        put(hw, self.cmd.mc, &encode_arr(id, words))?;
        let mut index = self.fence_value.wrapping_add(1);
        if index == 0 {
            index = 1;
        }
        let mb = Mailbox14 { addrs: self.addrs, first: self.fence_value == 0 };
        let slot = self.ring.push(hw, &mb, self.cmd.mc, self.fence.mc, index)?;
        self.fence_value = index;
        self.wptr = (slot + 1) * (psp::FRAME_BYTES / 4) % (self.ring.bytes() / 4);
        let start = hw.now_us();
        let tries = self.t.fence_us / self.t.fence_poll_us.max(1) + 1;
        let mut seen = 0;
        for _ in 0..=tries {
            seen = get32(hw, self.fence.mc)?;
            if seen == index || hw.now_us().saturating_sub(start) >= self.t.fence_us {
                break;
            }
            hw.delay_us(self.t.fence_poll_us.max(1));
        }
        if seen != index {
            self.wedged = true;
            klog::event!(Error, "gx", "rdna4.psp.fence", cmd = id, want = index, seen = seen; "no fence: the ring is wedged");
            return Err(PspError::Timeout { what: "fence", last: seen });
        }
        self.ring.complete(index);
        let mut r = [0u8; 20];
        let mut u = [0u8; 12];
        if !hw.mem_read(self.cmd.mc + psp::RESP_AT as u64, &mut r) || !hw.mem_read(self.cmd.mc + URESP_AT as u64, &mut u) {
            return Err(PspError::Mem { mc: self.cmd.mc });
        }
        let w = |b: &[u8], i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let resp = psp::Resp { status: w(&r, 0), session_id: w(&r, 4), fw_addr: u64::from(w(&r, 8)) | u64::from(w(&r, 12)) << 32, tmr_size: w(&r, 16) };
        Ok(Answer { resp, uresp: [w(&u, 0), w(&u, 4), w(&u, 8)] })
    }

    /// `psp_update_fw_reservation`'s two questions (amdgpu_psp.c:1091-1096).
    pub fn reserved(&mut self, hw: &mut dyn Hw) -> Result<[Reserved; 2], PspError> {
        let tol = [psp::PSP_ERR_UNKNOWN_COMMAND];
        let a = self.submit(hw, FB_FW_RESERV_ADDR, &[], &tol)?;
        let b = self.submit(hw, FB_FW_RESERV_EXT_ADDR, &[], &tol)?;
        Ok([Reserved::of(&a), Reserved::of(&b)])
    }

    /// `psp_v14_0_ring_destroy`: DESTROY_RINGS on C2PMSG_64, 20 ms, then Linux's answer check (`stop`). The
    /// buffers stay the PSP's until the answer: the caller gives the window back only after `Ok`.
    pub fn destroy(&mut self, hw: &mut dyn Hw) -> Result<(), PspError> {
        let r = stop(&self.addrs, hw);
        if r.is_ok() {
            (self.created, self.sent) = (false, false);
        }
        r
    }
}

/// DESTROY_RINGS as Linux checks it: the answer must be exactly 0x80000000 under 0x8000FFFF (psp_v14_0.c:264-266,
/// amdgpu_psp.h:64-65); a status means the ring is still up.
pub fn stop(addrs: &Addrs, hw: &mut dyn Hw) -> Result<(), PspError> {
    Mailbox14 { addrs: *addrs, first: false }.ring_stop(hw, &Timeouts::LINUX)
}

pub fn put(hw: &mut dyn Hw, mc: u64, data: &[u8]) -> Result<(), PspError> {
    if hw.mem_write(mc, data) { Ok(()) } else { Err(PspError::Mem { mc }) }
}

fn get32(hw: &mut dyn Hw, mc: u64) -> Result<u32, PspError> {
    let mut b = [0u8; 4];
    if hw.mem_read(mc, &mut b) { Ok(u32::from_le_bytes(b)) } else { Err(PspError::Mem { mc }) }
}
