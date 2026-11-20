//! GP-GPU2 fixtures shared by the host tests and the ktests: firmware files with the documented headers of the card's
//! files and made-up payloads (no real firmware in git), the list Linux would load from them written out by hand (so
//! the driver's list is checked, not copied), and the stick as a map.

use crate::gpucore::fwload::{FwSource, LoadError, stick_path};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

pub const PICTURE: u64 = 2560 * 1440 * 4;

/// A file: the common header (amdgpu_ucode.h:28-39), header fields at their bytes, the array at the next 256
/// bytes, `payload` bytes counting up from `seed`. `ucode_size` defaults to the payload.
pub fn file(major: u16, minor: u16, header: u32, payload: usize, seed: u8, ucode_size: Option<u32>, fields: &[(usize, u32)]) -> Vec<u8> {
    let array = header.next_multiple_of(256) as usize;
    let mut b = alloc::vec![0u8; array + payload];
    let len = b.len() as u32;
    let mut put = |at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    put(0, len);
    put(4, header);
    put(16, 0x0001_0203);
    put(20, ucode_size.unwrap_or(payload as u32));
    put(24, array as u32);
    for &(at, v) in fields {
        put(at, v);
    }
    b[8..10].copy_from_slice(&major.to_le_bytes());
    b[10..12].copy_from_slice(&minor.to_le_bytes());
    for (i, x) in b[array..].iter_mut().enumerate() {
        *x = seed.wrapping_add(i as u8).wrapping_mul(7) | 1;
    }
    b
}

pub const ARRAY: u32 = 256;

/// A v2.0 file of the secure processor (`psp_firmware_header_v2_0`, `ta_firmware_header_v2_0`): the count at 32,
/// 16-byte descriptors of (type, version, size) laid one after the other in the array.
fn described(parts: &[(u32, u32, usize)], seed: u8) -> Vec<u8> {
    let total: usize = parts.iter().map(|p| p.2).sum();
    let mut b = file(2, 0, 36 + 16 * parts.len() as u32, total, seed, None, &[(32, parts.len() as u32)]);
    let mut off = 0u32;
    for (i, &(kind, version, size)) in parts.iter().enumerate() {
        for (j, w) in [kind, version, off, size as u32].iter().enumerate() {
            b[36 + i * 16 + j * 4..40 + i * 16 + j * 4].copy_from_slice(&w.to_le_bytes());
        }
        off += size as u32;
    }
    b
}

/// The bytes of the part with this type in a v2.0 file made by `described`.
fn part_of(file: &[u8], kind: u32) -> Vec<u8> {
    let w = |at: usize| u32::from_le_bytes([file[at], file[at + 1], file[at + 2], file[at + 3]]);
    let i = (0..w(32) as usize).find(|i| w(36 + i * 16) == kind).expect("the fixture holds that part");
    let (off, size) = ((ARRAY + w(36 + i * 16 + 8)) as usize, w(36 + i * 16 + 12) as usize);
    file[off..off + size].to_vec()
}

/// The secure OS's file: v2.0 with the real file's eleven descriptors in its order (key database, system driver,
/// secure OS, SOC, interface and debug drivers, TOC, IP key manager, SPL table, RAS driver, register list), the
/// real sizes divided by 16 (the TOC whole), the secure OS's real version.
pub fn sos() -> Vec<u8> {
    described(&[(3, 0, 426), (2, 0, 4892), (1, 0x3A_1214, 7842), (7, 0, 6487), (8, 0, 2330), (9, 0, 3334), (4, 0, 2304), (11, 0, 803), (5, 0, 52), (10, 0, 794), (6, 0, 107)], 0x50)
}

pub fn toc_bytes() -> Vec<u8> {
    part_of(&sos(), 4)
}

/// Linux's nine bootloader loads from `sos()`, written out by hand: (command, the part's bytes), psp_hw_start's
/// order with `enum psp_bootloader_cmd`'s values (amdgpu_psp.h:95-109) and `enum psp_fw_type`'s types.
pub fn linux_boot() -> Vec<(u32, Vec<u8>)> {
    let s = sos();
    [(0x0008_0000, 3), (0x1000_0000, 5), (0x0001_0000, 2), (0x000B_0000, 7), (0x000D_0000, 8), (0x000C_0000, 9), (0x000E_0000, 10), (0x000F_0000, 11), (0x0002_0000, 1)]
        .into_iter()
        .map(|(command, kind)| (command, part_of(&s, kind)))
        .collect()
}

/// The trusted applications' file: v2.0 with the real file's four descriptors in its order (ASD, HDCP, DTM, RAS),
/// the real sizes divided by 16.
pub fn ta() -> Vec<u8> {
    described(&[(1, 0x21, 15474), (4, 0x22, 1576), (5, 0x23, 356), (3, 0x24, 788)], 0xA0)
}

/// The ten files of `fwlist::FILES`, in that order. The RLC file has the real one's shape: header v2.2 whose
/// save/restore control list is 0 bytes, so Linux loads no such image.
pub fn files() -> Vec<Vec<u8>> {
    // The engines' starts (`ucode_start_addr_lo` @52, `_hi` @56) are chosen so that Linux's arithmetic gives the
    // program counters of its trace (0xc00 and 0x1c000, init.txt lines 9157-9158): synthetic like the rest.
    // Their `ucode_version`s (@16) are the stick's of 18.11. (amdgpu_firmware_info: PFP 0xc94, ME 0xc26, MEC 0xda2),
    // which turn the cleaner shader on and with it MES's CHANGE_CONFIG (gfx_v12_0.c:1450-1458).
    let gfx = |seed, version| file(2, 0, 60, 8192 + 2048, seed, None, &[(16, version), (36, 8192), (40, ARRAY), (44, 2048), (48, ARRAY + 8192), (52, 0x3000), (56, 0x7_0000)]);
    alloc::vec![
        file(3, 0, 44, 4096, 0x10, None, &[(40, 4096)]),
        gfx(0x20, 0xC94),
        gfx(0x30, 0xC26),
        gfx(0x40, 0xDA2),
        // MES: both pipes start at `mes_uc_start_addr` (@56, @60), chosen so that Linux's arithmetic gives the
        // trace's program counter 0xfc001400 and high word 0x3fffffff (init.txt lines 9505-9506).
        file(1, 0, 64, 4096 + 1024, 0x60, None, &[(36, 4096), (40, ARRAY), (48, 1024), (52, ARRAY + 4096), (56, 0xF000_5000), (60, 0xFFFF_FFFF)]),
        file(1, 0, 48, 2048 + 1024, 0x70, None, &[(32, 2048), (40, 1024)]),
        file(2, 2, 172, 4096 + 2 * 256 + 2 * 1024, 0x80, Some(4096), &[
            (72, 0), (76, ARRAY), (80, 0), (84, ARRAY),
            (116, 0), (120, ARRAY + 4096), (132, 256), (136, ARRAY + 4096), (148, 256), (152, ARRAY + 4352),
            (156, 1024), (160, ARRAY + 4608), (164, 1024), (168, ARRAY + 5632),
        ]),
        file(2, 0, 64, 4096, 0x90, None, &[]),
        file(1, 0, 32, 12288, 0xB0, None, &[]),
        file(1, 0, 40, 8192 + 1024, 0xC0, None, &[(32, 8192), (36, 1024)]),
    ]
}

/// How many of `linux_images()` Linux loads before AUTOLOAD_RLC: up to RLC_G.
pub const BEFORE_AUTOLOAD: usize = 20;

/// What Linux 7.3.1 loads from `files()`, written out by hand: (LOAD_IP_FW type, bytes), in its order. The SMU's
/// firmware first; VCN and DMUB after RLC_G.
pub fn linux_images() -> Vec<(u32, Vec<u8>)> {
    let f = files();
    let a = ARRAY as usize;
    let cut = |i: usize, off: usize, len: usize| f[i][off..off + len].to_vec();
    alloc::vec![
        (18, cut(7, a, 4096)),
        (71, cut(0, a, 4096)),
        (87, cut(1, a, 8192)),
        (88, cut(2, a, 8192)),
        (89, cut(3, a, 8192)),
        (90, cut(1, a + 8192, 2048)),
        (92, cut(2, a + 8192, 2048)),
        (94, cut(3, a + 8192, 2048)),
        (95, cut(3, a + 8192, 2048)),
        (33, cut(4, a, 4096)),
        (34, cut(4, a + 4096, 1024)),
        (81, cut(4, a, 4096)),
        (82, cut(4, a + 4096, 1024)),
        (68, cut(5, a, 2048)),
        (69, cut(5, a + 2048, 1024)),
        (20, cut(6, a + 4096, 256)),
        (21, cut(6, a + 4352, 256)),
        (26, cut(6, a + 4608, 1024)),
        (48, cut(6, a + 5632, 1024)),
        (8, cut(6, a, 4096)),
        (13, cut(8, a, 12288)),
        (51, cut(9, a, 8192)),
    ]
}

/// `ta_ras_init_flags` for the measured card, by hand (amdgpu_psp.c:2452-2463): no poison mode, a discrete card,
/// one XCC, no disabled channel, no partition mode, eight UMC nodes none of them harvested, GDDR6.
pub const RAS_FLAGS: [u8; 16] = [0, 1, 1, 0, 0, 0, 0, 0, 0xFF, 0, 0, 0, 9, 0, 0, 0];

/// Everything the ring gets from the fixture files, in Linux's order.
pub fn expect() -> super::fakecmd::Expect {
    let (s, t) = (sos(), ta());
    super::fakecmd::Expect {
        toc: toc_bytes(),
        images: linux_images(),
        before_autoload: BEFORE_AUTOLOAD,
        asd: part_of(&t, 1),
        reg_list: part_of(&s, 6),
        tas: [part_of(&t, 3), part_of(&t, 4), part_of(&t, 5)],
        ras_flags: RAS_FLAGS,
    }
}

/// The stick as a map of paths.
pub struct Stick(pub BTreeMap<String, Vec<u8>>);

impl Stick {
    pub fn full() -> Stick {
        let mut m = BTreeMap::new();
        m.insert(stick_path(crate::rdna4::fwlist::SOS), sos());
        m.insert(stick_path(crate::rdna4::fwlist::TA), ta());
        for ((name, _), b) in crate::rdna4::fwlist::FILES.iter().zip(files()) {
            m.insert(stick_path(name), b);
        }
        Stick(m)
    }
}

impl FwSource for Stick {
    fn read(&mut self, path: &str) -> Result<Option<Vec<u8>>, LoadError> {
        Ok(self.0.get(path).cloned())
    }
}

/// The card cold, as Linux's PSP block meets it: no secure OS, the bootloader waiting for Linux's nine loads.
pub fn model() -> super::fakemodel::Model {
    super::fakemodel::Model::new(linux_boot(), expect())
}

/// The card with its secure OS already running.
pub fn warm() -> super::fakemodel::Model {
    model().warm()
}
