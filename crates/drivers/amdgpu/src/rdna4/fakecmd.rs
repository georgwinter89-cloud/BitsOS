//! GX-10 PS: the model's secure OS, the commands of the KM ring. It takes exactly what Linux sends this card
//! during its init, in Linux's order (`psp_load_fw`, amdgpu_psp.c:3595-3660; the 31 frames of the trace's section
//! psp-ring), each with Linux's arguments and the right bytes behind its address, and answers anything else with a
//! status:
//!
//! 1-2. FB_FW_RESERV_ADDR, FB_FW_RESERV_EXT_ADDR (only from secure OS 0x3A0E14 on, :1083-1086);
//! 3. LOAD_TOC with the TOC's bytes in the firmware buffer;
//! 4-23. LOAD_IP_FW: the SMU's firmware, then the images up to RLC_G, each with its type and bytes;
//! 24. AUTOLOAD_RLC right after RLC_G;
//! 25-26. LOAD_IP_FW: the images after RLC_G (VCN, DMUB);
//! 27. LOAD_ASD with the ASD in the firmware buffer and no shared buffer;
//! 28. LOAD_IP_FW of the register list (type 67) from the firmware buffer;
//! 29-31. LOAD_TA for RAS, HDCP and DTM, each from the firmware buffer with its own 16 KiB shared buffer in VRAM;
//!    RAS's holds Linux's init flags.
//!
//! What comes from the 1 MiB firmware buffer must lie there as `psp_copy_fw` leaves it: at a megabyte's start,
//! zeros behind it. Never SETUP_TMR or DESTROY_TMR (14.0.3 keeps its boot-time TMR).

use super::fakeboot::C64_READY;
use super::fakemodel::{MC_BASE, Model, TMR_SIZE, same};
use crate::gpucore::psp::{self, cmd};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rdna4_core::refuse;
use rdna4_core::regmap::Reg;

/// What Linux hands the secure OS over the ring on this card, from the stick's files.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Expect {
    pub toc: Vec<u8>,
    /// (LOAD_IP_FW type, bytes) in Linux's order, the SMU's first.
    pub images: Vec<(u32, Vec<u8>)>,
    /// How many of `images` go before AUTOLOAD_RLC (up to RLC_G).
    pub before_autoload: usize,
    pub asd: Vec<u8>,
    pub reg_list: Vec<u8>,
    /// RAS, HDCP, DTM.
    pub tas: [Vec<u8>; 3],
    /// `ta_ras_init_flags` as Linux fills them for this card.
    pub ras_flags: [u8; 16],
}

/// One command of Linux's sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Want {
    Reserv(u32),
    Toc,
    Image(usize),
    Autoload,
    Asd,
    RegList,
    Ta(usize),
}

/// A command's answer: `psp_gfx_resp` (psp_gfx_if.h:448-461) without its status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Ans {
    session: u32,
    fw_addr: u64,
    tmr_size: u32,
    uresp: [u32; 3],
}

const SHARED_BYTES: u32 = 0x4000;
const RAS_STRUCT: usize = 16 + 1024 + 1024;

impl Model {
    /// Linux's sequence for this model's secure OS version and files.
    pub fn script(&self) -> Vec<Want> {
        let mut v = Vec::new();
        if self.sos_version >= 0x003A_0E14 {
            v.extend([Want::Reserv(0x50), Want::Reserv(0x51)]);
        }
        v.push(Want::Toc);
        let n = self.expect.before_autoload.min(self.expect.images.len());
        v.extend((0..n).map(Want::Image));
        v.push(Want::Autoload);
        v.extend((n..self.expect.images.len()).map(Want::Image));
        v.extend([Want::Asd, Want::RegList, Want::Ta(0), Want::Ta(1), Want::Ta(2)]);
        v
    }

    /// The command Linux sends next on this ring, `None` when all were answered.
    pub fn next(&self) -> Option<Want> {
        self.script().get(self.done).copied()
    }

    /// One frame: the command buffer, its answer, the fence.
    pub(super) fn frame(&mut self, at: u64) {
        let cmd_mc = u64::from(self.word(at)) | u64::from(self.word(at + 4)) << 32;
        let fence_mc = u64::from(self.word(at + 12)) | u64::from(self.word(at + 16)) << 32;
        let value = self.word(at + 20);
        if !self.psp_may_have(cmd_mc, psp::CMD_RESP_BYTES as u64) || !self.psp_may_have(fence_mc, 4) {
            self.refused.push(format!("a frame naming memory outside VRAM ({cmd_mc:#x}, {fence_mc:#x})"));
            return;
        }
        let id = self.word(cmd_mc + psp::CMD_ID_AT as u64);
        self.commands.push(id);
        let mut a = [0u32; 6];
        for (i, w) in a.iter_mut().enumerate() {
            *w = self.word(cmd_mc + psp::CMD_AT as u64 + 4 * i as u64);
        }
        let (status, ans) = match self.command(id, a) {
            Ok(x) => x,
            Err(why) => {
                self.refused.push(why);
                (0xFFFF_0001, Ans::default())
            }
        };
        let r = cmd_mc + psp::RESP_AT as u64;
        for (off, w) in [(0, status), (4, ans.session), (8, ans.fw_addr as u32), (12, (ans.fw_addr >> 32) as u32), (16, ans.tmr_size), (64, ans.uresp[0]), (68, ans.uresp[1]), (72, ans.uresp[2])] {
            self.put_word(r + off, w);
        }
        if self.hang_on != Some(id) {
            self.put_word(fence_mc, value);
        }
    }

    /// The firmware buffer at `mc` as `psp_copy_fw` leaves it for `bytes`.
    fn copied(&self, mc: u64, len: u32, bytes: &[u8], what: &str) -> Result<(), String> {
        if len as usize != bytes.len() {
            return Err(format!("{what}: {len} bytes where Linux hands over {}", bytes.len()));
        }
        if !mc.is_multiple_of(psp::PRI_BUF_BYTES) || !self.free_megabyte(mc) {
            return Err(format!("{what} from {mc:#x}: not the 1 MiB firmware buffer in free VRAM"));
        }
        let mut got = alloc::vec![0u8; psp::PRI_BUF_BYTES as usize];
        self.peek(mc - MC_BASE, &mut got);
        if got[..bytes.len()] != *bytes || got[bytes.len()..].iter().any(|b| *b != 0) {
            return Err(format!("{what}: the firmware buffer does not hold those bytes and zeros behind them"));
        }
        Ok(())
    }

    /// A command's status and answer, or why Linux would not have sent it.
    fn command(&mut self, id: u32, a: [u32; 6]) -> Result<(u32, Ans), String> {
        let addr = u64::from(a[0]) | u64::from(a[1]) << 32;
        if matches!(id, cmd::SETUP_TMR | cmd::DESTROY_TMR) {
            return Err(format!("command {id:#x}: 14.0.3 keeps its boot-time TMR, Linux sends none"));
        }
        let Some(want) = self.next() else { return Err(format!("command {id:#x} after Linux's last")) };
        let e = self.expect.clone();
        let mut status = 0;
        let ans = match (want, id) {
            (Want::Reserv(0x50), 0x50) => {
                let base = self.reserved_at.unwrap_or(self.vram_bytes() - u64::from(self.reserved));
                Ans { uresp: [(base >> 32) as u32, base as u32, self.reserved], ..Ans::default() }
            }
            (Want::Reserv(0x51), 0x51) => {
                let base = self.reserved_ext_at.unwrap_or(self.vram_bytes() - u64::from(self.reserved) - u64::from(self.reserved_ext));
                Ans { uresp: [(base >> 32) as u32, base as u32, self.reserved_ext], ..Ans::default() }
            }
            (Want::Toc, cmd::LOAD_TOC) => {
                self.copied(addr, a[2], &e.toc, "LOAD_TOC")?;
                Ans { tmr_size: TMR_SIZE, ..Ans::default() }
            }
            (Want::Image(n), cmd::LOAD_IP_FW) => {
                let (ty, bytes) = &e.images[n];
                if a[3] != *ty {
                    return Err(format!("LOAD_IP_FW number {n}: type {} where Linux loads type {ty}", a[3]));
                }
                // The first image is the SMU's firmware, which starts the graphics block anew.
                if n == 0 && self.gc.context0 {
                    return Err(String::from("LOAD_IP_FW of the SMU's firmware while GFXHUB's context 0 is on: Linux loads it over a block that ran only at a resume, after its fini switched that hub off (gfx_v12_0.c:3917)"));
                }
                // ... and MES, whose program the autoload starts anew (Linux halts it first, mes_v12_0_kiq_hw_fini).
                if n == 0 && self.mes.running.iter().any(|r| *r) {
                    return Err(String::from("LOAD_IP_FW of the SMU's firmware while a MES pipe runs: Linux halts MES first at a suspend (mes_v12_0_kiq_hw_fini, mes_v12_0.c:2018-2034)"));
                }
                // ... and with it the copy engines, whose firmware the autoload starts.
                if n == 0 && self.sdma.any_on() {
                    return Err(String::from("LOAD_IP_FW of the SMU's firmware while a copy engine's ring is on: Linux loads it over engines that ran only at a resume, after its fini stopped them (sdma_v7_0_hw_fini, sdma_v7_0.c:1422-1434)"));
                }
                if a[2] as usize != bytes.len() || !self.psp_may_have(addr, u64::from(a[2])) {
                    return Err(format!("LOAD_IP_FW type {ty}: {} bytes at {addr:#x}", a[2]));
                }
                let mut got = alloc::vec![0u8; bytes.len()];
                self.peek(addr - MC_BASE, &mut got);
                if got != *bytes {
                    return Err(format!("LOAD_IP_FW type {ty}: not that image's bytes"));
                }
                // `GFX_FW_TYPE_SMU`: the power controller's firmware starts anew.
                if *ty == 18 {
                    self.pmfw_reloaded();
                }
                self.mes_image_loaded(*ty);
                Ans { fw_addr: 0x1000 * (n as u64 + 1), ..Ans::default() }
            }
            (Want::Autoload, cmd::AUTOLOAD_RLC) => {
                self.gc_autoloaded();
                Ans::default()
            }
            (Want::Asd, 0x04) => {
                self.copied(addr, a[2], &e.asd, "LOAD_ASD")?;
                if a[3..] != [0, 0, 0] {
                    return Err(format!("LOAD_ASD with a shared buffer ({:x?}): Linux gives it none", &a[3..]));
                }
                Ans { session: 0x0A5D_0001, ..Ans::default() }
            }
            (Want::RegList, cmd::LOAD_IP_FW) => {
                if a[3] != 67 {
                    return Err(format!("LOAD_IP_FW type {} where Linux loads the register list (67)", a[3]));
                }
                self.copied(addr, a[2], &e.reg_list, "LOAD_IP_FW register list")?;
                Ans::default()
            }
            (Want::Ta(n), cmd::LOAD_TA) => {
                self.copied(addr, a[2], &e.tas[n], "LOAD_TA")?;
                let shared = u64::from(a[3]) | u64::from(a[4]) << 32;
                let off = shared.wrapping_sub(MC_BASE);
                let in_picture = off < self.picture.end && off.saturating_add(u64::from(SHARED_BYTES)) > self.picture.start;
                if a[5] != SHARED_BYTES || !shared.is_multiple_of(0x1000) || !self.psp_may_have(shared, u64::from(SHARED_BYTES)) || in_picture {
                    return Err(format!("LOAD_TA number {n}: shared buffer {:#x} bytes at {shared:#x}, not 16 KiB on a page of free VRAM", a[5]));
                }
                if n == 0 {
                    let mut got = [0u8; RAS_STRUCT];
                    self.peek(off, &mut got);
                    if got[16..32] != e.ras_flags || got[..16].iter().chain(&got[32..]).any(|b| *b != 0) {
                        return Err(format!("LOAD_TA RAS: the shared buffer does not hold Linux's init flags ({:x?})", &got[16..32]));
                    }
                }
                if let Some((_, st)) = self.ta_status.filter(|(i, _)| *i == n) {
                    status = st;
                }
                Ans { session: 0x7A00_0001 + n as u32, ..Ans::default() }
            }
            (want, id) => return Err(format!("command {id:#x} where Linux sends {want:?}: not Linux's order")),
        };
        self.done += 1;
        Ok((status, ans))
    }
}

/// The ring's registers: create, destroy and the write pointer that hands the secure OS its frames.
impl Model {
    pub(super) fn psp_write(&mut self, reg: Reg, byte: u32, v: u32) -> bool {
        use rdna4_core::regmap as rm;
        let p = self.regs.psp;
        if byte == p.c2pmsg_69 || byte == p.c2pmsg_70 || byte == p.c2pmsg_71 {
            if self.ring.is_some() {
                return self.refuse(format!("{} written while a ring runs", reg.name));
            }
            if !self.ready_seen {
                return self.refuse(format!("{} written before the ready bit was read", reg.name));
            }
            self.staged.push(byte);
            match byte {
                b if b == p.c2pmsg_69 => self.c69 = v,
                b if b == p.c2pmsg_70 => self.c70 = v,
                _ => self.c71 = v,
            }
            return true;
        }
        if same(reg, rm::PSP_C2PMSG_64) {
            let staged = core::mem::take(&mut self.staged);
            return match v {
                refuse::GFX_CTRL_INIT_GPCOM_RING => {
                    let mc = u64::from(self.c70) << 32 | u64::from(self.c69);
                    if self.sol == 0 {
                        self.refuse(String::from("ring create on a secure OS that does not run"))
                    } else if self.ring.is_some() {
                        self.refuse(String::from("a second ring"))
                    } else if !self.ready_seen {
                        self.refuse(String::from("ring create without waiting for the ready bit"))
                    } else if staged != [p.c2pmsg_69, p.c2pmsg_70, p.c2pmsg_71] {
                        self.refuse(format!("ring create after {staged:x?}, not 69, 70, 71"))
                    } else if self.c71 != psp::RING_BYTES || !mc.is_multiple_of(0x1000) || !self.psp_may_have(mc, u64::from(self.c71)) {
                        self.refuse(format!("ring at {mc:#x}, {:#x} bytes: not a 4 KiB ring inside VRAM and outside the GART table", self.c71))
                    } else {
                        self.ring = Some((mc, self.c71));
                        // The answer carries the command in bits 23:16, as the card answered Linux (0x80c20000).
                        let answer = if self.create_late { 0 } else { C64_READY | v };
                        (self.c67, self.slot, self.done, self.c64, self.ready_seen) = (0, 0, 0, answer, false);
                        true
                    }
                }
                refuse::GFX_CTRL_DESTROY_RINGS => {
                    if self.ring.is_none() {
                        return self.refuse(String::from("destroy without a ring"));
                    }
                    // No trace shows a destroy: its answer is taken to be built as create's.
                    match self.destroy_status {
                        Some(st) => self.c64 = C64_READY | v | u32::from(st),
                        None => (self.ring, self.c64) = (None, C64_READY | v),
                    }
                    true
                }
                other => self.refuse(format!("ring control {other:#x}")),
            };
        }
        if byte == p.c2pmsg_67 {
            let Some((mc, size)) = self.ring else { return self.refuse(String::from("a frame without a ring")) };
            if self.unflushed {
                return self.refuse(String::from("a frame not flushed through HDP first (amdgpu_psp.c:3961)"));
            }
            let frame_dw = psp::FRAME_BYTES / 4;
            if !v.is_multiple_of(frame_dw) || v >= size / 4 {
                return self.refuse(format!("write pointer {v:#x} not at a frame inside the ring"));
            }
            self.c67 = v;
            let frames = size / psp::FRAME_BYTES;
            while self.slot != v / frame_dw {
                let at = mc + u64::from(self.slot * psp::FRAME_BYTES);
                self.frame(at);
                self.slot = (self.slot + 1) % frames;
            }
            return true;
        }
        self.refuse(format!("{} is no register a driver writes here", reg.name))
    }
}
