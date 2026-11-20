//! What goes over the ring after the TOC, in Linux's order (`psp_load_fw`, amdgpu_psp.c:3625-3660), frame for frame
//! the trace's commands 4 to 31 (section psp-ring): `fw` (the SMU's firmware, then the walk up to RLC_G),
//! `autoload` (AUTOLOAD_RLC right after RLC_G), `fwrest` (the walk's rest: VCN and DMCUB) and `ta` (the ASD, the
//! secure OS file's register list, then the RAS, HDCP and DTM applications).
//!
//! Linux copies every image into one buffer before the first command (`amdgpu_ucode_init_bo`,
//! amdgpu_ucode.c:1176-1215), each at the next page; so does `fw`, in the card's window. What the secure OS reads
//! from the 1 MiB firmware buffer (the ASD, the register list, the applications) is put there as `psp_copy_fw` does.

use super::fwlist::{self, Id};
use super::pspfiles::{self, SosFile, TaFile};
use super::ring14::{put, Answer};
use super::stage::{Ctx, Session, Stop, TaLoaded, need_pri, need_ring, psp_stop, read_file, stop};
use crate::bo::PAGE;
use crate::gpucore::fwload::FwSource;
use crate::gpucore::hwid;
use crate::gpucore::psp::{BoAlloc, Hw, cmd};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// psp_gfx_if.h:92.
pub const LOAD_ASD: u32 = 0x04;
/// `GFX_FW_TYPE_REG_LIST` (psp_gfx_if.h:273).
pub const FW_TYPE_REG_LIST: u32 = 67;
/// `PSP_RAS_SHARED_MEM_SIZE`, `PSP_HDCP_SHARED_MEM_SIZE`, `PSP_DTM_SHARED_MEM_SIZE` (amdgpu_psp.h:72-74); the ASD
/// has none (`PSP_ASD_SHARED_MEM_SIZE` 0, :70).
pub const TA_SHARED_BYTES: u32 = 0x4000;
/// `ras_status` in `struct ta_ras_shared_memory` (ta_ras_if.h:215-222: four dwords, of which it is the third,
/// then the two 1024-byte messages).
const RAS_STATUS_AT: u64 = 8;

fn load_ip_fw(s: &mut Session, hw: &mut dyn Hw, id: Id, at: u64, size: u32, out: &mut Vec<String>) -> Result<(), Stop> {
    let a = need_ring(s)?.submit(hw, cmd::LOAD_IP_FW, &[at as u32, (at >> 32) as u32, size, id.fw_type()], &[]).map_err(|e| {
        out.push(format!("LOAD_IP_FW {id:?} (type {}) failed", id.fw_type()));
        psp_stop(e)
    })?;
    out.push(format!("LOAD_IP_FW {id:?} type {} {size} bytes from mc {at:#x}: placed at {:#x}", id.fw_type(), a.resp.fw_addr));
    s.loaded.push((id, at, size, a.resp.fw_addr));
    if id == Id::Smc {
        // The secure processor answered the load of the PMFW's firmware: a new PMFW runs, and what the one
        // before it was told, and owes, is gone. Only here: a load that failed or got no answer leaves it
        // unknown which PMFW runs, and the one before may still be at work on a message.
        s.pmfw.reloaded();
        s.smu_asked = false;
        // Linux's hw_fini cancels the software CTF check with the PMFW it belonged to (amdgpu_smu.c:2159).
        if let Some(ih) = s.nb.ih.as_mut() {
            ih.reader.thermal.reloaded();
        }
    }
    Ok(())
}

/// `psp_load_non_psp_fw` (amdgpu_psp.c:3515-3593) up to RLC_G. The SMU's firmware goes first, always: Linux reloads
/// it on a cold start of this card (`fwlist`).
pub(super) fn fw(s: &mut Session, c: &Ctx, hw: &mut dyn Hw, files: &mut dyn FwSource, out: &mut Vec<String>) -> Result<(), Stop> {
    if s.fw_done || !s.loaded.is_empty() {
        return Err(stop("Reihenfolge stimmt nicht", "fw ran for this ring (to its end, or it stopped after its first image): once per ring"));
    }
    // Before any access: a second `fw` in one boot would load the firmware anew under copy engines that run.
    super::sdmastage::before_fw(&s.sdma)?;
    super::messtage::before_fw(&s.mes)?;
    super::cpstage::before_fw(&s.cp)?;
    let mut bytes = Vec::new();
    for (plain, _) in fwlist::FILES {
        bytes.push(read_file(files, fwlist::name_for(plain, c.facts.device, c.facts.revision))?);
    }
    let refs: Vec<&[u8]> = bytes.iter().map(Vec::as_slice).collect();
    let list = fwlist::list(&refs).map_err(|e| stop("eine Datei auf dem Stick passt nicht", format!("{e:?}")))?;
    let total: u64 = list.iter().map(|u| u64::from(u.size()).next_multiple_of(PAGE)).sum();
    need_ring(s)?;
    let (Some(bump), Some(l)) = (s.bump.as_mut(), s.layout.as_ref()) else { return Err(stop("zuerst den Befehlskanal anlegen", "no window")) };
    let (_, range) = bump.alloc(total, PAGE).map_err(|e| stop("kein Platz im Speicher der Karte", format!("{e:?}")))?;
    let mut at = l.mc(range.start);
    let mut placed = Vec::new();
    for u in &list {
        put(hw, at, &bytes[u.file][u.range.clone()]).map_err(psp_stop)?;
        placed.push((u.id, at, u.size()));
        at += u64::from(u.size()).next_multiple_of(PAGE);
    }
    let first = fwlist::before_autoload(&list);
    s.pending = placed.split_off(first);
    // The SMU's firmware and the RLC's autoload start the graphics block anew: on a second ring of one boot
    // GFXHUB is switched off before the first image goes out, and what `gcboot` and `gchub` did is void from
    // here (`gcstage::before_fw`: Linux's fini before its resume).
    super::gcstage::before_fw(&mut s.gc, c, hw, out)?;
    // From here the power controller's start is due again too, also when its image is not answered below
    // (`load_ip_fw` then keeps the old PMFW's record): no `gcboot` on a load that stopped half-way.
    s.pmfw.reload_began = true;
    for (id, at, size) in placed {
        load_ip_fw(s, hw, id, at, size, out)?;
    }
    s.fw_done = true;
    Ok(())
}

/// `psp_rlc_autoload_start` (amdgpu_psp.c:3902-3915), right after RLC_G (:3581-3588). Whether the autoload came
/// through is GFX's question (`gfx_v12_0_wait_for_rlc_autoload_complete`, the trace's section gc-boot), not this
/// stage's: Linux's next access here is VCN's frame.
pub(super) fn autoload(s: &mut Session, hw: &mut dyn Hw, out: &mut Vec<String>) -> Result<(), Stop> {
    if s.loaded.last().map(|l| l.0) != Some(Id::RlcG) || s.autoload {
        return Err(stop("Reihenfolge stimmt nicht", "autoload follows the image RLC_G, once per ring"));
    }
    need_ring(s)?.submit(hw, cmd::AUTOLOAD_RLC, &[], &[]).map_err(psp_stop)?;
    s.autoload = true;
    out.push(String::from("autoload: AUTOLOAD_RLC answered"));
    Ok(())
}

/// The walk's images after RLC_G (their `AMDGPU_UCODE_ID` is higher): VCN, then the display's DMCUB.
pub(super) fn fwrest(s: &mut Session, hw: &mut dyn Hw, out: &mut Vec<String>) -> Result<(), Stop> {
    if s.rest_done || s.loaded.last().map(|l| l.0) != Some(Id::RlcG) {
        return Err(stop("Reihenfolge stimmt nicht", "fwrest ran for this ring (to its end, or it stopped after an image): once per ring"));
    }
    need_ring(s)?;
    while !s.pending.is_empty() {
        let (id, at, size) = s.pending[0];
        load_ip_fw(s, hw, id, at, size, out)?;
        s.pending.remove(0);
    }
    s.rest_done = true;
    Ok(())
}

/// `ta_ras_init_flags` (ta_ras_if.h:151-159) as `psp_ras_initialize` fills them for this card
/// (amdgpu_psp.c:2452-2463), from the card's own discovery table where Linux takes them from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RasFlags {
    pub poison_mode_en: u8,
    pub dgpu_mode: u8,
    pub xcc_mask: u16,
    pub channel_dis_num: u8,
    pub nps_mode: u8,
    pub active_umc_mask: u32,
    pub vram_type: u8,
}

impl RasFlags {
    pub fn of(c: &Ctx) -> RasFlags {
        let d = &c.att.discovery;
        // amdgpu_discovery.c:1854-1857 and :918-920, :938-939: one bit per UMC entry, less the harvested ones.
        let nodes = d.ips.iter().filter(|i| i.hw_id == hwid::UMC).count() as u32;
        let harvested = d.harvest.iter().flatten().filter(|h| h.hw_id == hwid::UMC).fold(0u64, |m, h| m | 1u64.checked_shl(u32::from(h.instance)).unwrap_or(0));
        let all = 1u64.checked_shl(nodes).map_or(u64::MAX, |b| b - 1);
        // amdgpu_discovery.c:2133-2143: `m_half_use` of a version 1 MALL table.
        let half_use = d.mall.as_ref().filter(|m| m.major == 1).and_then(|m| m.fields.iter().find(|(n, _)| *n == "m_half_use")).map_or(0, |(_, v)| *v);
        RasFlags {
            // Poison mode needs both DF and UMC to report it (amdgpu_ras.c:3399-3412); DF 4.15's functions have no
            // such query (df_v4_15.c), so it stays off.
            poison_mode_en: 0,
            // A discrete card: not connected to the CPU, no APU (amdgpu_psp.c:2454-2455).
            dgpu_mode: 1,
            // One GC instance (amdgpu_discovery.c:1860-1861).
            xcc_mask: 1,
            channel_dis_num: (half_use.count_ones() * 2) as u8,
            // GMC 12 has no `query_mem_partition_mode` (gmc_v12_0.c).
            nps_mode: 0,
            active_umc_mask: (all & !harvested) as u32,
            // What Linux read from the VBIOS on this card (`rdna4_core::devinfo`, the libdrm capture).
            vram_type: rdna4_core::devinfo::VRAM_TYPE as u8,
        }
    }

    /// The struct's bytes as the compiler lays them out for the secure OS (natural alignment, 16 bytes).
    pub fn bytes(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0] = self.poison_mode_en;
        b[1] = self.dgpu_mode;
        b[2..4].copy_from_slice(&self.xcc_mask.to_le_bytes());
        b[4] = self.channel_dis_num;
        b[5] = self.nps_mode;
        b[8..12].copy_from_slice(&self.active_umc_mask.to_le_bytes());
        b[12] = self.vram_type;
        b
    }
}

fn zero(hw: &mut dyn Hw, mc: u64, bytes: usize) -> Result<(), Stop> {
    // A `static`: a `const` of this size may be copied to the stack where it is used (as `ring14`'s zeros).
    static Z: [u8; 4096] = [0; 4096];
    for at in (0..bytes).step_by(Z.len()) {
        put(hw, mc + at as u64, &Z[..Z.len().min(bytes - at)]).map_err(psp_stop)?;
    }
    Ok(())
}

/// `psp_ta_load` (amdgpu_psp.c:1836-1866): the application into the firmware buffer, then LOAD_TA or LOAD_ASD with
/// the buffer, the length and the shared buffer (`psp_prep_ta_load_cmd_buf` :1777-1791).
fn ta_load(s: &mut Session, hw: &mut dyn Hw, id: u32, app: &[u8], shared: (u64, u32), strict: bool) -> Result<Answer, Stop> {
    need_ring(s)?;
    let pri = need_pri(s)?;
    let mc = pri.buf.mc;
    pri.load(hw, app).map_err(psp_stop)?;
    let words = [mc as u32, (mc >> 32) as u32, app.len() as u32, shared.0 as u32, (shared.0 >> 32) as u32, shared.1];
    let ring = need_ring(s)?;
    if strict { ring.submit(hw, id, &words, &[]) } else { ring.submit_any(hw, id, &words) }.map_err(psp_stop)
}

/// `psp_asd_initialize`, `psp_rl_load` and the three optional applications, as `psp_load_fw` runs them
/// (amdgpu_psp.c:3620-3670): frames 27 to 31 of the trace. An application the secure OS answers with a status is
/// Linux's to log and pass over, and so it is here (the line says so); the ASD and the register list are not
/// optional. A file that lacks any of the five stops the stage before the first of them (Linux would skip it and
/// send fewer commands than its own trace shows).
pub(super) fn ta(s: &mut Session, c: &Ctx, hw: &mut dyn Hw, files: &mut dyn FwSource, out: &mut Vec<String>) -> Result<(), Stop> {
    if s.ta_done || !s.tas.is_empty() {
        return Err(stop("Reihenfolge stimmt nicht", "ta ran for this ring (to its end, or it stopped after an application): once per ring"));
    }
    let bad = |name: &str, e: String| stop("eine Datei auf dem Stick passt nicht", format!("{name}: {e}"));
    let ta_name = fwlist::name_for(fwlist::TA, c.facts.device, c.facts.revision);
    let ta_bytes = read_file(files, ta_name)?;
    let ta = TaFile::parse(&ta_bytes).map_err(|e| bad(ta_name, format!("{e:?}")))?;
    let sos_name = fwlist::name_for(fwlist::SOS, c.facts.device, c.facts.revision);
    let sos_bytes = read_file(files, sos_name)?;
    let sos = SosFile::parse(&sos_bytes).map_err(|e| bad(sos_name, format!("{e:?}")))?;
    let app = |kind: u32, what: &str| ta.app(kind).map(|d| &ta_bytes[d.range.clone()]).ok_or_else(|| bad(ta_name, format!("no {what} (descriptor type {kind})")));
    let asd = app(pspfiles::TA_ASD, "ASD")?;
    let optional = [("RAS", app(pspfiles::TA_RAS, "RAS application")?), ("HDCP", app(pspfiles::TA_HDCP, "HDCP application")?), ("DTM", app(pspfiles::TA_DTM, "DTM application")?)];
    let rl = sos.part(pspfiles::TYPE_RL).map(|d| &sos_bytes[d.range.clone()]).ok_or_else(|| bad(sos_name, String::from("no register list (descriptor type 6)")))?;
    need_ring(s)?;

    // psp_asd_initialize (amdgpu_psp.c:1681-1707): no shared buffer.
    let a = ta_load(s, hw, LOAD_ASD, asd, (0, 0), true)?;
    out.push(format!("LOAD_ASD {} bytes: session {:#x}", asd.len(), a.resp.session_id));
    s.tas.push(TaLoaded { name: "ASD", session: a.resp.session_id, status: a.resp.status });

    // psp_rl_load (amdgpu_psp.c:1177-1205): the register list of the secure OS's file, as an IP firmware.
    let pri = need_pri(s)?;
    let mc = pri.buf.mc;
    pri.load(hw, rl).map_err(psp_stop)?;
    need_ring(s)?.submit(hw, cmd::LOAD_IP_FW, &[mc as u32, (mc >> 32) as u32, rl.len() as u32, FW_TYPE_REG_LIST], &[]).map_err(psp_stop)?;
    out.push(format!("LOAD_IP_FW register list (type {FW_TYPE_REG_LIST}) {} bytes", rl.len()));

    for (name, bytes) in optional {
        // psp_ta_init_shared_buf (amdgpu_psp.c:1793-1806): 16 KiB, page aligned; in the window, as every buffer.
        let (Some(bump), Some(l)) = (s.bump.as_mut(), s.layout.as_ref()) else { return Err(stop("zuerst den Befehlskanal anlegen", "no window")) };
        let (_, range) = bump.alloc(u64::from(TA_SHARED_BYTES), PAGE).map_err(|e| stop("kein Platz im Speicher der Karte", format!("{e:?}")))?;
        let shared = l.mc(range.start);
        // Linux clears only RAS's struct (:2450) and leaves the rest as the memory was; here all of it is zero.
        zero(hw, shared, TA_SHARED_BYTES as usize)?;
        if name == "RAS" {
            let flags = RasFlags::of(c);
            put(hw, shared + 16, &flags.bytes()).map_err(psp_stop)?;
            out.push(format!("RAS init flags: {flags:?}"));
        }
        let a = ta_load(s, hw, cmd::LOAD_TA, bytes, (shared, TA_SHARED_BYTES), false)?;
        let mut line = format!("LOAD_TA {name} {} bytes, shared buffer at mc {shared:#x}: session {:#x}, status {:#x}", bytes.len(), a.resp.session_id, a.resp.status);
        if name == "RAS" {
            let mut st = [0u8; 4];
            if hw.mem_read(shared + RAS_STATUS_AT, &mut st) {
                line.push_str(&format!(", RAS init status {:#x}", u32::from_le_bytes(st)));
            }
        }
        if a.resp.status != 0 {
            line.push_str(" (not accepted; Linux logs that and goes on, so does this)");
        }
        out.push(line);
        s.tas.push(TaLoaded { name, session: a.resp.session_id, status: a.resp.status });
    }
    s.ta_done = true;
    Ok(())
}
