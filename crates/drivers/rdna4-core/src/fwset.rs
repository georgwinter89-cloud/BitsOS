//! The firmware this card asks for. Sizes and hashes are of the unpacked files from `linux-firmware-amdgpu`
//! 1:20261019-1, recomputed on 5.11.; the files themselves never enter the repository, and their licence forbids
//! disassembly, so nothing here looks past a name.

/// A:`amdgpu_ucode.c`:33: `kicker_device_list`. These boards get other PSP, SMU, RLC and IMU files.
const KICKER_BOARDS: [(u16, u8); 2] = [(0x744B, 0x00), (0x7551, 0xC8)];

/// `amdgpu_is_kicker_fw`, A:`amdgpu_ucode.c`:1432: device and revision together.
pub fn is_kicker(device: u16, revision: u8) -> bool {
    KICKER_BOARDS.contains(&(device, revision))
}

/// `amdgpu_uni_mes` defaults to 1 (A:`amdgpu_drv.c`:227): one file serves both pipes (A:`amdgpu_mes.c`:780-783).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mes {
    Unified,
    TwoFiles,
}

/// Why a file is in the set, which decides whether a driver loads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Use {
    /// Handed to the secure processor.
    Load,
    /// Only for `Mes::Unified`.
    MesUnified,
    /// Only for `Mes::TwoFiles`.
    MesTwoFiles,
    /// Read only by `load_type` RLC backdoor (A:`gfx_v12_0.c`:573), not under the default PSP load.
    NotNeeded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fw {
    pub file: &'static str,
    /// The name a kicker board asks for instead, if it differs. Those files were not hashed: this board is not one.
    pub kicker_file: Option<&'static str>,
    pub bytes: u32,
    pub sha256: &'static str,
    pub usage: Use,
}

const fn fw(file: &'static str, bytes: u32, sha256: &'static str, usage: Use) -> Fw {
    Fw { file, kicker_file: None, bytes, sha256, usage }
}

const fn kicker(file: &'static str, kicker: &'static str, bytes: u32, sha256: &'static str, usage: Use) -> Fw {
    Fw { file, kicker_file: Some(kicker), bytes, sha256, usage }
}

pub const FILES: [Fw; 15] = [
    kicker(
        "amdgpu/psp_14_0_3_sos.bin",
        "amdgpu/psp_14_0_3_sos_kicker.bin", // A:amdgpu_psp.c:4175
        435_632,
        "28469a0857c813c54a0492423cdf0b0caf757428400036377e19c47e5af62478",
        // The card starts cold without a secure OS: its bootloader parts, the secure OS, the TOC and the register
        // list all come from this file (`plan/spec/gpu-rdna4.md` pieces 7 and 8).
        Use::Load,
    ),
    kicker(
        "amdgpu/psp_14_0_3_ta.bin",
        "amdgpu/psp_14_0_3_ta_kicker.bin", // A:amdgpu_psp.c:4456
        291_372,
        "fff8b0b56f21bf1d34c8d1fcf6cc903c2dbb5ef88eb06157c9f08baf95124883",
        Use::Load,
    ),
    kicker(
        "amdgpu/smu_14_0_3.bin",
        "amdgpu/smu_14_0_3_kicker.bin", // pm/swsmu/smu14/smu_v14_0.c:85
        333_236,
        "7624724231b0986b59f59705e7c854c482193f6b3b591ebdda58cb62af81c010",
        Use::Load,
    ),
    fw(
        "amdgpu/gc_12_0_1_pfp.bin",
        159_616,
        "490bcd26a62e389b3f58acf5c087d324d2324b3188797ff6a36b7a6cdf1ca9ff",
        Use::Load,
    ),
    fw(
        "amdgpu/gc_12_0_1_me.bin",
        267_600,
        "fd5eb0f205a3bb7ec41f42e83a54bdc69151706b02bbee073fa775b8270e4bec",
        Use::Load,
    ),
    fw(
        "amdgpu/gc_12_0_1_mec.bin",
        259_584,
        "83fe5e7bffe639ec9771219cde5c9809689168f769b147c86dfd5a1ca991c1a2",
        Use::Load,
    ),
    kicker(
        "amdgpu/gc_12_0_1_rlc.bin",
        "amdgpu/gc_12_0_1_rlc_kicker.bin", // A:gfx_v12_0.c:628
        150_848,
        "6ba4459532246a5c415d3cb33c9b1248294e48f67b827e2accb292a8d1a5c0ec",
        Use::Load,
    ),
    kicker(
        "amdgpu/gc_12_0_1_imu.bin",
        "amdgpu/gc_12_0_1_imu_kicker.bin", // A:imu_v12_0.c:53
        132_352,
        "b3b301fb636efc77b63ce4d2ced0f90c851d03c19681852faa45598e6f5773fd",
        Use::Load,
    ),
    // On disk a symlink to gc_12_0_0_uni_mes; the two names are one file here.
    fw(
        "amdgpu/gc_12_0_1_uni_mes.bin",
        299_408,
        "309d60ad4a0210f8a68e27cae88beddb22b5ae900d70848b1e8ca0eac9d1decd",
        Use::MesUnified,
    ),
    fw(
        "amdgpu/gc_12_0_1_mes.bin",
        643_536,
        "306c99718564ff613813b0c15a8e0dd1797488fac01de60702fac0d079c1731d",
        Use::MesTwoFiles,
    ),
    fw(
        "amdgpu/gc_12_0_1_mes1.bin",
        611_920,
        "b873c0e354b94e6ae10831dd883013689a4fb04259c4cd46a9b9ddef4c29ddd5",
        Use::MesTwoFiles,
    ),
    fw("amdgpu/sdma_7_0_1.bin", 47_104, "73c29e1c1714ebc95d2221ba56e187910902891593010653bf9518937e414a59", Use::Load),
    // The name comes from the file on disk and from the version rule in `ipfacts::display_version`: the DC sources
    // that spell it out are not in the reference tree.
    fw(
        "amdgpu/dcn_4_0_1_dmcub.bin",
        368_288,
        "74b567655ac89bfe03389b660e973dc3c067aebd464c6389a071662cfbe6237f",
        Use::Load,
    ),
    // Same for this one: vcn_v5_0_0.c does not spell it out, the name is built from the version.
    fw("amdgpu/vcn_5_0_0.bin", 440_400, "9a0ab35bf157c835cbbc4a207ba819ea0111e2a65fe7f8c20910ffff3122f903", Use::Load),
    fw(
        "amdgpu/gc_12_0_1_toc.bin",
        2_048,
        "edad661e1c7bd350a86285513fbdeeb33ca4e80c06f1ea7f37d344ba1e2761dd",
        Use::NotNeeded,
    ),
];

/// One file as a given board wants it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chosen {
    pub file: &'static str,
    /// Size and hash, or `None` for a kicker name, which nobody has checked.
    pub expected: Option<(u32, &'static str)>,
}

fn wanted(f: &Fw, mes: Mes) -> bool {
    match f.usage {
        Use::Load => true,
        Use::MesUnified => mes == Mes::Unified,
        Use::MesTwoFiles => mes == Mes::TwoFiles,
        Use::NotNeeded => false,
    }
}

/// What a driver hands to the card, in this table's order. The order of loading is the secure processor's list, which
/// is not built yet.
pub fn load_set(device: u16, revision: u8, mes: Mes) -> impl Iterator<Item = Chosen> {
    let kicker = is_kicker(device, revision);
    FILES.iter().filter(move |f| wanted(f, mes)).map(move |f| match f.kicker_file {
        Some(name) if kicker => Chosen { file: name, expected: None },
        _ => Chosen { file: f.file, expected: Some((f.bytes, f.sha256)) },
    })
}
