//! GX-8a: what INFO reports from the chip itself instead of the captured profile (`drm::profile_780m`): the active
//! CUs and render backends per shader array (harvesting differs between chips of one name, Mesa sizes its shader
//! and tile setup from them), GB_ADDR_CONFIG, which rings really started, the firmware versions really loaded.
//! The rest of DEV_INFO (VA layout, cache sizes, clocks) is fixed for the chip or comes from a VBIOS table V2 does
//! not read yet, and stays as captured.

use super::TscRegs;
use crate::gpucore::disc::Discovery;
use crate::gpucore::gfxring::GfxAddrs;
use crate::gpucore::hwid;
use crate::gpucore::psp::Hw;
use crate::gpucore::ucode::{FwFile, FwSet, Kind};
use crate::ipmod::gfx11::{self, R};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use drm::gpu::{ChipProfile, Firmware, HW_IP_NUM};
use drm::uapi::*;

/// CC_GC_SHADER_ARRAY_CONFIG.INACTIVE_WGPS, as gc_11_0_0_sh_mask.h says (the user register's field is the same).
const INACTIVE_WGPS_SHIFT: u32 = 16;
/// CC_RB_BACKEND_DISABLE.BACKEND_DISABLE (gc_11_0_0_sh_mask.h: bits 31:4).
const BACKEND_DISABLE_SHIFT: u32 = 4;
/// CGTS_TCC_DISABLE.TCC_DISABLE (bits 31:16; amdgpu_kms.c tcc_disabled_mask, gfx_v11_0.c:1525).
const TCC_DISABLE_SHIFT: u32 = 16;

/// regGB_ADDR_CONFIG in GC's segment 0 (gc_11_0_0_offset.h).
pub const GB_ADDR_CONFIG_SEG0: u32 = 0x13de;
const GOLDEN_TSC_COUNT_UPPER: u32 = 0x2d;
const GOLDEN_TSC_COUNT_LOWER: u32 = 0x2e;
/// AMDGPU_IDS_FLAGS_TMZ (amdgpu_drm.h).
const IDS_FLAGS_TMZ: u64 = 0x4;

#[derive(Clone, Debug)]
pub struct ChipFacts {
    pub dev_info: drm_amdgpu_info_device,
    pub firmware: Vec<Firmware>,
    pub rings: [u32; HW_IP_NUM],
    pub gb_addr_config: u32,
}

/// The PCI identity and what started, from the bring-up.
#[derive(Clone, Copy, Debug)]
pub struct Started {
    pub device: u16,
    pub revision: u8,
    pub gfx: bool,
    pub compute: bool,
    pub sdma: bool,
}

/// `AMDGPU_INFO_FW_VERSION`'s table: every entry the captured profile has, with what was loaded in this bring-up
/// (Linux: `gfx.*_fw_version`, `*_feature_version`, amdgpu_kms.c:253); 0/0 for firmware V2 did not load (VCN until
/// the video work, the TAs, the PSP's own). MES and SMC answer at run time: `with_mes`, `with_smc`. Files: the common header's `ucode_version` and the dword after the
/// common header (`ucode_feature_version`); checked against linux-firmware's gc_11_0_1_{me,pfp,mec,rlc}.bin and
/// sdma_6_0_1.bin: ME 0x67/35, PFP 0x6f/35, MEC 0x46/35, RLC 0x8c/1, SDMA 0x18/60, the captured values.
pub fn loaded_firmware(profile: &ChipProfile, fw: &FwSet) -> Vec<Firmware> {
    let file = |k: Kind| fw.get(k).map(|f: &FwFile| (f.header.ucode_version, feature(f)));
    profile
        .firmware
        .iter()
        .map(|e| {
            let v = match e.fw_type {
                AMDGPU_INFO_FW_GFX_ME => file(Kind::Me),
                AMDGPU_INFO_FW_GFX_PFP => file(Kind::Pfp),
                AMDGPU_INFO_FW_GFX_MEC if e.index == 0 => file(Kind::Mec),
                AMDGPU_INFO_FW_GFX_RLC => file(Kind::Rlc),
                AMDGPU_INFO_FW_SDMA if e.index == 0 => file(Kind::Sdma),
                AMDGPU_INFO_FW_IMU => file(Kind::Imu).map(|(v, _)| (v, 0)),
                AMDGPU_INFO_FW_TOC => file(Kind::Toc),
                _ => None,
            };
            let (ver, feature) = v.unwrap_or((0, 0));
            Firmware { fw_type: e.fw_type, index: e.index, ver, feature }
        })
        .collect()
}

impl ChipFacts {
    /// The answer's line about the chip: what the desk compares with the capture.
    pub fn describe(&self) -> String {
        let d = &self.dev_info;
        let fw = |ty, i| self.firmware.iter().find(|f| f.fw_type == ty && f.index == i).map_or((0, 0), |f| (f.ver, f.feature));
        let (me, pfp, mec, sdma, mes, kiq) = (fw(AMDGPU_INFO_FW_GFX_ME, 0), fw(AMDGPU_INFO_FW_GFX_PFP, 0), fw(AMDGPU_INFO_FW_GFX_MEC, 0), fw(AMDGPU_INFO_FW_SDMA, 0), fw(AMDGPU_INFO_FW_MES, 0), fw(AMDGPU_INFO_FW_MES_KIQ, 0));
        format!(
            "backend chip: {} CUs {:x?}, RBs {:#x} ({}), TCC disabled {:#x}, GB_ADDR_CONFIG {:#x}, rings GFX {} compute {} SDMA {}; firmware ME {:#x}/{} PFP {:#x}/{} MEC {:#x}/{} SDMA {:#x}/{} MES {:#x}/{} KIQ {:#x}/{}",
            d.cu_active_number, &d.cu_bitmap[0][..2], d.enabled_rb_pipes_mask, d.num_rb_pipes, d.tcc_disabled_mask, self.gb_addr_config,
            self.rings[AMDGPU_HW_IP_GFX as usize], self.rings[AMDGPU_HW_IP_COMPUTE as usize], self.rings[AMDGPU_HW_IP_DMA as usize],
            me.0, me.1, pfp.0, pfp.1, mec.0, mec.1, sdma.0, sdma.1, mes.0, mes.1, kiq.0, kiq.1
        )
    }
}

/// The SMC entry as Linux fills it (amdgpu_kms.c: `adev->pm.fw_version`, feature 0): what PMFW answered to
/// GetPmfwVersion in the GC stage (`gfxup::GfxReport::pmfw`; the capture's value is the same message's). `None`: no
/// GC stage has asked yet, the entry stays as it was.
pub fn with_smc(mut table: Vec<Firmware>, pmfw: Option<u32>) -> Vec<Firmware> {
    if let Some(v) = pmfw {
        for f in table.iter_mut().filter(|f| f.fw_type == AMDGPU_INFO_FW_SMC) {
            f.ver = v;
            f.feature = 0;
        }
    }
    table
}

/// MES's entries as it reports them (`mes::MesReport::versions`, CP_MES_GP3_LO per pipe; amdgpu_mes.h: version bits
/// 11:0, feature bits 31:24): scheduler, KIQ.
pub fn with_mes(mut table: Vec<Firmware>, mes: (Option<u32>, Option<u32>)) -> Vec<Firmware> {
    for f in &mut table {
        let v = match f.fw_type {
            AMDGPU_INFO_FW_MES => mes.0,
            AMDGPU_INFO_FW_MES_KIQ => mes.1,
            _ => None,
        };
        if let Some(x) = v {
            f.ver = x & 0xfff;
            f.feature = x >> 24;
        }
    }
    table
}

/// SMUIO's GOLDEN_TSC_COUNT pair by byte (smuio_13_0_6_offset.h, base index 1, the header gfx_v11_0.c uses).
pub fn tsc_regs(d: &Discovery) -> Option<TscRegs> {
    let byte = |dw| d.reg(hwid::SMUIO, 0, 1, dw).and_then(|b: u32| b.checked_mul(4));
    Some(TscRegs { upper: byte(GOLDEN_TSC_COUNT_UPPER)?, lower: byte(GOLDEN_TSC_COUNT_LOWER)? })
}

/// `ucode_feature_version`, the first dword after the common header of a GFX, RLC or SDMA file.
fn feature(f: &FwFile) -> u32 {
    let at = crate::gpucore::ucode::COMMON_BYTES;
    f.bytes.get(at..at + 4).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Reads the chip's shape on the kept owner, as `gfx_v11_0_get_cu_info` and `gfx_v11_0_setup_rb` do, with
/// GRBM_GFX_INDEX back on broadcast at the end. `Err`: a register did not answer, or the chip is not the profile's.
pub fn read_facts(o: &mut dyn Hw, a: &GfxAddrs, profile: &ChipProfile, gb_addr_config: u32, s: Started, firmware: Vec<Firmware>) -> Result<ChipFacts, String> {
    let mut d = profile.dev_info;
    if u32::from(s.device) != d.device_id {
        return Err(format!("PCI device {:#x} is not the profile's {:#x} ({})", s.device, d.device_id, profile.name));
    }
    d.pci_rev = u32::from(s.revision);
    let rd = |o: &mut dyn Hw, r: R| a.byte(r).and_then(|b| o.reg_read(b)).ok_or_else(|| format!("{r:?} did not read"));
    let index = a.byte(R::GrbmGfxIndex).ok_or("no GRBM_GFX_INDEX")?;
    let (se_n, sh_n) = (d.num_shader_engines.min(4), d.num_shader_arrays_per_engine.min(4));
    let wgp_mask = (1u32 << (d.num_cu_per_sh >> 1).min(16)) - 1;
    let mut bitmap = [[0u32; 4]; 4];
    let mut active = 0;
    let mut read = || -> Result<(), String> {
        for se in 0..se_n {
            for sh in 0..sh_n {
                if !o.reg_write(index, gfx11::gfx_index(se, sh)) {
                    return Err(String::from("GRBM_GFX_INDEX refused"));
                }
                let off = rd(&mut *o, R::CcGcShaderArrayConfig)? | rd(&mut *o, R::GcUserShaderArrayConfig)?;
                let wgps = !(off >> INACTIVE_WGPS_SHIFT) & wgp_mask;
                // One active WGP is two CUs (gfx_v11_0_get_cu_active_bitmap_per_sh).
                let cus = (0..16).filter(|w| wgps >> w & 1 != 0).fold(0u32, |m, w| m | 3 << (2 * w));
                bitmap[se as usize][sh as usize] = cus;
                active += cus.count_ones();
            }
        }
        Ok(())
    };
    let r = read();
    let back = o.reg_write(index, gfx11::GFX_INDEX_BROADCAST);
    r?;
    if !back {
        return Err(String::from("GRBM_GFX_INDEX not back on broadcast"));
    }
    d.cu_bitmap = bitmap;
    d.cu_active_number = active;
    // gfx_v11_0_get_rb_active_bitmap.
    let rb_off = (rd(o, R::CcRbBackendDisable)? | rd(o, R::GcUserRbBackendDisable)?) >> BACKEND_DISABLE_SHIFT;
    let rb_mask = (1u32 << d.num_rb_pipes.min(31)) - 1;
    let rbs = rb_mask & !rb_off;
    d.enabled_rb_pipes_mask = rbs;
    d.num_rb_pipes = rbs.count_ones();
    d.tcc_disabled_mask = u64::from((rd(o, R::CgtsTccDisable)? | rd(o, R::CgtsUserTccDisable)?) >> TCC_DISABLE_SHIFT);
    // V2 sets up no trusted memory for programs (the captured kernel did): RADV must not offer protected memory.
    d.ids_flags &= !IDS_FLAGS_TMZ;
    let mut rings = [0; HW_IP_NUM];
    rings[AMDGPU_HW_IP_GFX as usize] = u32::from(s.gfx);
    rings[AMDGPU_HW_IP_COMPUTE as usize] = u32::from(s.compute);
    rings[AMDGPU_HW_IP_DMA as usize] = u32::from(s.sdma);
    klog::event!(Info, "gx", "hw.chip", cus = active, rbs = klog::Hex(u64::from(rbs)), gb_addr_config = klog::Hex(u64::from(gb_addr_config)));
    Ok(ChipFacts { dev_info: d, firmware, rings, gb_addr_config })
}
