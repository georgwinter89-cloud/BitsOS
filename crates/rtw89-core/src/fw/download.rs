//! The firmware download into the chip's WLAN CPU (JS js/fw.mjs), as Linux
//! rtw89 fw.c and mac.c do it for an AX chip on PCIe: CPU off and on with
//! FWDL enabled, the header H2C, the sections in pieces of 2020 bytes on CH12,
//! the status check, five tries with a fail dump after each failed one; plus
//! the CH12 resync before a retry that the JS driver added (not Linux).

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::bus::{Bus, BusError, Width, field, prep};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::pci::{self, PciRings};
use crate::tables::regs::*;

use super::h2c::h2c_hdr_fwdl;
use super::parse::{Firmware, FwSection};
use super::{FailDump, FwEvent, FwState};

/// read_poll_timeout_atomic(..., 1, FWDL_WAIT_CNT): FWDL_WAIT_CNT us (400 ms) in ms
pub const FWDL_WAIT_MS: u16 = FWDL_WAIT_CNT.div_ceil(1000) as u16;
/// rtw89_fw_download's tries
pub const FWDL_RETRIES: u32 = 5;
/// pci::fwcmd's wait for the chip to fetch one CH12 buffer (the JS's 50 ms)
pub const CH12_WAIT_MS: u16 = 50;
/// rtw89_fw_prog_cnt_dump's program counter samples
pub const PC_DUMPS: usize = 15;
/// mdelay(5) before the status check
const FWDL_SETTLE_US: u32 = 5000;
/// fsleep(10) between two program counter samples
const PC_DUMP_GAP_US: u32 = 10;

// The JS driver retries device errors (RtwError) and its own FwErrors; a lost connection to
// the chip (Gone, Transport) and programming errors (Invalid, an impossible DMA address)
// end the download at once, without a fail dump.
fn retryable(e: &Rtw89Error) -> bool {
    !matches!(
        e,
        Rtw89Error::Bus(BusError::Gone)
            | Rtw89Error::Bus(BusError::Transport)
            | Rtw89Error::Invalid { .. }
            | Rtw89Error::BadDmaAddress { .. }
    )
}

// Linux: rtw89_mac_disable_cpu_ax (mac.c:4146)
pub fn disable_cpu<B: Bus + ?Sized>(bus: &mut B, st: &mut FwState) -> Rtw89Result<()> {
    st.ready = false; // clear RTW89_FLAG_FW_RDY
    bus.clr32(R_AX_PLATFORM_ENABLE, B_AX_WCPU_EN)?;
    bus.clr32(
        R_AX_WCPU_FW_CTRL,
        B_AX_WCPU_FWDL_EN | B_AX_H2C_PATH_RDY | B_AX_FWDL_PATH_RDY,
    )?;
    bus.clr32(R_AX_SYS_CLK_CTRL, B_AX_CPU_CLK_EN)?;
    // rtw89_disable_fw_watchdog (mac.c:4131): 8852B is an 885xB chip, the APB wrapper is toggled
    bus.clr32(R_AX_PLATFORM_ENABLE, B_AX_APB_WRAP_EN)?;
    bus.set32(R_AX_PLATFORM_ENABLE, B_AX_APB_WRAP_EN)?;
    bus.clr32(R_AX_PLATFORM_ENABLE, B_AX_PLATFORM_EN)?;
    bus.set32(R_AX_PLATFORM_ENABLE, B_AX_PLATFORM_EN)?;
    Ok(())
}

// Linux: rtw89_mac_enable_cpu_ax (mac.c:4161)
pub fn enable_cpu<B: Bus + ?Sized>(
    bus: &mut B,
    st: &mut FwState,
    boot_reason: u32,
    dlfw: bool,
) -> Rtw89Result<()> {
    let pe = bus.r32(R_AX_PLATFORM_ENABLE)?;
    if pe & B_AX_WCPU_EN != 0 {
        return Err(Rtw89Error::WcpuAlreadyOn {
            reg: R_AX_PLATFORM_ENABLE,
            value: pe,
        });
    }
    for reg in [
        R_AX_UDM1,
        R_AX_UDM2,
        R_AX_HALT_H2C_CTRL,
        R_AX_HALT_C2H_CTRL,
        R_AX_HALT_H2C,
        R_AX_HALT_C2H,
    ] {
        bus.w32(reg, 0)?;
    }
    bus.set32(R_AX_SYS_CLK_CTRL, B_AX_CPU_CLK_EN)?;
    // read, clear the enable/ready bits, status to initial, FWDL_EN, write: the new value
    // depends on the old one only, so one read-modify-write (as the JS)
    let m = B_AX_WCPU_FWDL_EN | B_AX_H2C_PATH_RDY | B_AX_FWDL_PATH_RDY | B_AX_WCPU_FWDL_STS_MASK;
    let bits = prep(B_AX_WCPU_FWDL_STS_MASK, RTW89_FWDL_INITIAL_STATE)
        | if dlfw { B_AX_WCPU_FWDL_EN } else { 0 };
    bus.rmw(Width::W32, R_AX_WCPU_FW_CTRL, m, bits & m)?;
    // 8852B is an 885xB chip: IDMEM size configuration 2
    bus.mask32(R_AX_SEC_CTRL, B_AX_SEC_IDMEM_SIZE_CONFIG_MASK, 0x2)?;
    bus.mask(
        Width::W16,
        R_AX_BOOT_REASON,
        B_AX_BOOT_REASON_MASK,
        boot_reason,
    )?;
    bus.set32(R_AX_PLATFORM_ENABLE, B_AX_WCPU_EN)?;
    if !dlfw {
        bus.delay_us(FWDL_SETTLE_US)?;
        check_rdy(bus, st)?;
    }
    Ok(())
}

// Linux: rtw89_fwdl_check_path_ready_ax (mac.c:7419): an 8-bit read as Linux's rtw89_read8
fn check_path_ready<B: Bus + ?Sized>(bus: &mut B, h2c: bool) -> Rtw89Result<()> {
    let (bit, path) = if h2c {
        (B_AX_H2C_PATH_RDY, "H2C")
    } else {
        (B_AX_FWDL_PATH_RDY, "FWDL")
    };
    bus.poll(Width::W8, R_AX_WCPU_FW_CTRL, bit, bit, FWDL_WAIT_MS)
        .map_err(|e| match e {
            BusError::PollTimeout { addr, last, .. } => Rtw89Error::FwPathNotReady {
                path,
                reg: addr,
                value: last,
            },
            e => e.into(),
        })
}

// Linux: rtw89_fw_check_rdy (fw.c:106) with rtw89_fw_get_rdy_ax (mac.c:7411). The poll's
// last value is not handed back by every bus: one more 8-bit read gives the status for the
// error (as the JS; one access more than Linux, on the failure path only).
pub fn check_rdy<B: Bus + ?Sized>(bus: &mut B, st: &mut FwState) -> Rtw89Result<()> {
    let sts = B_AX_WCPU_FWDL_STS_MASK;
    match bus.poll(
        Width::W8,
        R_AX_WCPU_FW_CTRL,
        sts,
        prep(sts, RTW89_FWDL_WCPU_FW_INIT_RDY),
        FWDL_WAIT_MS,
    ) {
        Ok(()) => {
            st.ready = true; // set RTW89_FLAG_FW_RDY
            Ok(())
        }
        Err(BusError::PollTimeout { .. }) => {
            let v = u32::from(bus.r8(R_AX_WCPU_FW_CTRL)?);
            Err(Rtw89Error::FwStatus {
                reg: R_AX_WCPU_FW_CTRL,
                value: v,
                status: field(v, sts),
            })
        }
        Err(e) => Err(e.into()),
    }
}

/// The text Linux prints for a FWDL status (rtw89_fw_check_rdy)
pub fn fwdl_status_text(status: u32) -> Option<&'static str> {
    match status {
        RTW89_FWDL_CHECKSUM_FAIL => Some("fw checksum fail"),
        RTW89_FWDL_SECURITY_FAIL => Some("fw security fail"),
        RTW89_FWDL_CV_NOT_MATCH => Some("fw cv not match"),
        _ => None,
    }
}

fn put_field(buf: &mut [u8], at: usize, mask: u32, v: u32) -> Rtw89Result<()> {
    let Some(s) = buf.get_mut(at..at + 4) else {
        return Err(Rtw89Error::FwFile {
            what: "fw header shorter than its sections",
            value: at as u32,
        });
    };
    let old = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
    s.copy_from_slice(&((old & !mask) | prep(mask, v)).to_le_bytes());
    Ok(())
}

// Linux: __rtw89_fw_download_hdr (fw.c:1729) with __rtw89_fw_download_tweak_hdr_v0
// (fw.c:1676): the header H2C's bytes. A copy: the tweak (part size into w7, len_override
// into a section's w1) never touches the file.
pub fn fw_hdr_h2c(st: &FwState, fw: &Firmware) -> Rtw89Result<Vec<u8>> {
    let info = &fw.suit.info;
    let len = info.base_hdr_len() as usize;
    let data = fw.suit_data();
    let mut hdr = data
        .get(..len)
        .ok_or(Rtw89Error::FwFile {
            what: "fw header shorter than its sections",
            value: len as u32,
        })?
        .to_vec();
    put_field(&mut hdr, 28, FW_HDR_W7_PART_SIZE, info.part_size)?;
    for (i, s) in info.sections.iter().enumerate() {
        if s.len_override != 0 {
            put_field(
                &mut hdr,
                32 + i * 16 + 4,
                FWSECTION_HDR_W1_SEC_SIZE,
                s.len_override,
            )?;
        }
    }
    let h = h2c_hdr_fwdl(
        st,
        FWCMD_TYPE_H2C,
        H2C_CAT_MAC,
        H2C_CL_MAC_FWDL,
        H2C_FUNC_MAC_FWHDR_DL,
        len as u32,
    );
    let mut out = Vec::with_capacity(8 + len);
    out.extend_from_slice(&h);
    out.extend_from_slice(&hdr);
    Ok(out)
}

// Linux: rtw89_fw_download_hdr (fw.c:1785)
fn download_hdr<B: Bus + ?Sized>(
    bus: &mut B,
    rings: &mut PciRings,
    st: &FwState,
    fw: &Firmware,
) -> Rtw89Result<()> {
    let h2c = fw_hdr_h2c(st, fw)?;
    pci::fwcmd(bus, rings, &h2c, false, CH12_WAIT_MS)?; // rtw89_h2c_tx(skb, false)
    check_path_ready(bus, false)?;
    bus.w32(R_AX_HALT_H2C_CTRL, 0)?;
    bus.w32(R_AX_HALT_C2H_CTRL, 0)?;
    Ok(())
}

/// One FWDL piece: `len` bytes of the suit from `at`; `key` = (suit offset, length) of the
/// MSS key copied over its end (secure boot only, never with this chip)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Piece {
    pub at: usize,
    pub len: usize,
    pub key: Option<(usize, usize)>,
}

// Linux: __rtw89_fw_download_main (fw.c:1810): the pieces of one section (none when it is
// ignored); a len_override above the section's length is ignored (fw.c:1825). Lazy, so a
// file with a tiny part size costs no piece list in memory.
pub fn section_pieces(sec: &FwSection, part_size: u32) -> Pieces {
    let mut residue = sec.len as usize;
    if sec.len_override != 0 && sec.len_override <= sec.len {
        residue = sec.len_override as usize;
    }
    if sec.ignore || part_size == 0 {
        residue = 0;
    }
    let part = part_size as usize;
    let key_len = sec.key_len as usize;
    let copy_key =
        sec.key_addr.is_some() && key_len != 0 && residue <= part && sec.len as usize >= key_len;
    Pieces {
        at: sec.offset,
        residue,
        part,
        key: sec.key_addr.filter(|_| copy_key).map(|k| (k, key_len)),
    }
}

/// The pieces of one section, in download order (see [`section_pieces`])
#[derive(Debug, Clone)]
pub struct Pieces {
    at: usize,
    residue: usize,
    part: usize,
    key: Option<(usize, usize)>,
}

impl Iterator for Pieces {
    type Item = Piece;

    fn next(&mut self) -> Option<Piece> {
        if self.residue == 0 {
            return None;
        }
        let len = self.residue.min(self.part);
        let p = Piece {
            at: self.at,
            len,
            key: self.key,
        };
        self.at += len;
        self.residue -= len;
        Some(p)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.residue.div_ceil(self.part.max(1));
        (n, Some(n))
    }
}

impl ExactSizeIterator for Pieces {}

/// A piece's bytes out of the suit (a copy only when a key goes over its end)
pub fn piece_bytes<'a>(suit: &'a [u8], p: &Piece) -> Rtw89Result<Cow<'a, [u8]>> {
    let out_of = || Rtw89Error::FwFile {
        what: "fw piece past the suit",
        value: p.at as u32,
    };
    let bytes = suit
        .get(p.at..p.at.checked_add(p.len).ok_or_else(out_of)?)
        .ok_or_else(out_of)?;
    let Some((k, kl)) = p.key else {
        return Ok(Cow::Borrowed(bytes));
    };
    let key = suit
        .get(k..k.checked_add(kl).ok_or_else(out_of)?)
        .ok_or_else(out_of)?;
    let mut v = bytes.to_vec();
    let start = p.len.checked_sub(kl).ok_or_else(out_of)?;
    v[start..].copy_from_slice(key);
    Ok(Cow::Owned(v))
}

// Linux: rtw89_fw_download_main (fw.c:1887); AX checks the status later, in
// __rtw89_fw_download. Each piece waits for the chip to fetch it (pci::fwcmd), so the
// JS's flush every 64 pieces (an MCP script size limit) has no counterpart here.
fn download_main<B: Bus + ?Sized>(
    bus: &mut B,
    rings: &mut PciRings,
    fw: &Firmware,
) -> Rtw89Result<u32> {
    let suit = fw.suit_data();
    let mut n = 0u32;
    for sec in &fw.suit.info.sections {
        for p in section_pieces(sec, fw.suit.info.part_size) {
            let bytes = piece_bytes(suit, &p)?;
            pci::fwcmd(bus, rings, &bytes, true, CH12_WAIT_MS)?; // rtw89_h2c_tx(skb, true)
            n += 1;
        }
    }
    bus.flush()?;
    Ok(n)
}

// Linux: rtw89_fw_download_suit (fw.c:1957). rtw89_fwdl_secure_idmem_share_mode_ax returns
// at once without secure boot; the 8922A write does not apply.
fn download_suit<B: Bus + ?Sized>(
    bus: &mut B,
    rings: &mut PciRings,
    fw: &Firmware,
    st: &mut FwState,
) -> Rtw89Result<()> {
    check_path_ready(bus, true)?;
    download_hdr(bus, rings, st, fw)?;
    st.pieces = download_main(bus, rings, fw)?;
    Ok(())
}

// Linux: rtw89_fw_dl_fail_dump (fw.c:1943) with rtw89_fw_prog_cnt_dump (fw.c:1918)
pub fn dl_fail_dump<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<FailDump> {
    let fw_ctrl = bus.r32(R_AX_WCPU_FW_CTRL)?;
    let boot_dbg = bus.r32(R_AX_BOOT_DBG)?;
    bus.w32(
        R_AX_DBG_CTRL,
        prep(B_AX_DBG_SEL0, FW_PROG_CNTR_DBG_SEL) | prep(B_AX_DBG_SEL1, FW_PROG_CNTR_DBG_SEL),
    )?;
    bus.mask32(R_AX_SYS_STATUS1, B_AX_SEL_0XC0_MASK, MAC_DBG_SEL)?;
    let mut pc = Vec::with_capacity(PC_DUMPS);
    for _ in 0..PC_DUMPS {
        pc.push(bus.r32(R_AX_DBG_PORT_SEL)?);
        bus.delay_us(PC_DUMP_GAP_US)?;
    }
    Ok(FailDump {
        fw_ctrl,
        boot_dbg,
        pc,
    })
}

// Linux: __rtw89_fw_download (fw.c:1994), type NORMAL(_CE), include_bb false (8852B: bbmcu_nr 0)
fn download_once<B: Bus + ?Sized>(
    bus: &mut B,
    rings: &mut PciRings,
    fw: &Firmware,
    st: &mut FwState,
) -> Rtw89Result<()> {
    disable_cpu(bus, st)?;
    enable_cpu(bus, st, 0, true)?; // its error returns without the dump, as Linux
    let r = (|| {
        download_suit(bus, rings, fw, st)?;
        st.h2c_seq = 0;
        st.rec_seq = 0;
        st.h2c_counter = 0;
        st.c2h_counter = 0;
        // rpwm/cpwm sequence numbers belong to power saving, which BitsOS never enters
        bus.delay_us(FWDL_SETTLE_US)?; // mdelay(5)
        check_rdy(bus, st) // RTW89_FWDL_CHECK_FREERTOS_DONE
    })();
    let Err(e) = r else {
        return Ok(());
    };
    if !retryable(&e) {
        return Err(e);
    }
    match dl_fail_dump(bus) {
        Ok(d) => {
            st.fail_dump = Some(d.clone());
            st.push(FwEvent::FailDump(d));
        }
        Err(e2) if retryable(&e2) => st.push(FwEvent::FailDumpFailed(e2)),
        Err(e2) => return Err(e2),
    }
    Err(e)
}

// Not in Linux, which kicks every H2C at once and never waits for CH12 (JS fw.mjs
// `ch12Resync`, spec/wlan.md §2.2): pci::fwcmd moves the host index before its wait, so after
// a timeout the ring is ahead of the chip. Before the next try the ring goes back to the
// host index the chip saw and every kicked buffer must be fetched within 50 ms; otherwise
// the download stops at once ("CH12 DMA stuck", "link gone") instead of four useless tries.
fn ch12_resync<B: Bus + ?Sized>(
    bus: &mut B,
    rings: &mut PciRings,
    err: &Rtw89Error,
) -> Rtw89Result<()> {
    match pci::fwcmd_resync(bus, rings, CH12_WAIT_MS) {
        Ok(_) => Ok(()),
        Err(Rtw89Error::RingIndex { reg, value, .. }) => Err(Rtw89Error::FwLinkGone {
            reg,
            value,
            before: Box::new(err.clone()),
        }),
        Err(Rtw89Error::Bus(BusError::PollTimeout { addr, last, .. })) => {
            let host = rings.tx("CH12").map_or(0, |r| r.wp);
            Err(Rtw89Error::Ch12Stuck {
                reg: addr,
                host,
                value: last,
                before: Box::new(err.clone()),
            })
        }
        Err(e) => Err(e),
    }
}

// Linux: rtw89_fw_download (fw.c:2043): up to five tries, each followed by the CH12 resync
// when it failed; sets `st.ready`, `st.pieces`, `st.tries`. The last try's error is returned
// (or the resync's, which ends the tries early). Each failed try is an event.
pub fn download<B: Bus + ?Sized>(
    bus: &mut B,
    rings: &mut PciRings,
    fw: &Firmware,
    st: &mut FwState,
) -> Rtw89Result<()> {
    let mut retry = 0;
    loop {
        let e = match download_once(bus, rings, fw, st) {
            Ok(()) => {
                st.tries = retry + 1;
                return Ok(());
            }
            Err(e) if !retryable(&e) => return Err(e),
            Err(e) => e,
        };
        st.push(FwEvent::TryFailed {
            try_no: retry + 1,
            error: e.clone(),
        });
        ch12_resync(bus, rings, &e)?;
        retry += 1;
        if retry >= FWDL_RETRIES {
            return Err(e);
        }
    }
}
