//! GX-7a firmware headers, the load list and the loader: the synthetic 780M set gives Linux's order; every
//! refusal has its reason; random damage never panics; the real linux-firmware files (if the host has them)
//! parse and give the same list.

use crate::gpucore::fwload::{self, FwSource, LoadError, stick_path};
use crate::gpucore::pspfake::{self, EXPECT_780M, MapSource};
use crate::gpucore::ucode::{self, FwFile, FwSet, Kind, ListOpts, UcodeId, Why};
use crate::ipmod;
use std::collections::BTreeMap;
use std::string::String;
use std::vec::Vec;

fn synth() -> BTreeMap<String, Vec<u8>> {
    pspfake::synthetic_780m()
}

fn path(name: &str) -> String {
    stick_path(&std::format!("amdgpu/{name}.bin"))
}

fn set_with(opts: &ListOpts, files: BTreeMap<String, Vec<u8>>) -> Result<FwSet, LoadError> {
    fwload::load(&pspfake::plan_780m().files(), &mut MapSource(files), opts).map(|(s, _)| s)
}

fn file(kind: Kind, b: Vec<u8>) -> Result<FwFile, Why> {
    FwFile::new(kind, String::from("x"), b)
}

fn put(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

#[test]
fn synthetic_780m_list_is_linux_order() {
    let opts = ListOpts::PSP13_0_4;
    let s = set_with(&opts, synth()).unwrap();
    let list = s.load_list(&opts).unwrap();
    let ids: Vec<_> = list.iter().map(|u| u.id).collect();
    assert_eq!(ids, EXPECT_780M);
    let types: Vec<u32> = list.iter().map(|u| u.id.fw_type()).collect();
    assert_eq!(types, [71, 72, 2, 1, 4, 33, 34, 81, 82, 68, 69, 20, 21, 26, 48, 25, 8]);
    let size = |id| list.iter().find(|u| u.id == id).unwrap().size();
    assert_eq!(size(UcodeId::CpMec1), 0x2400 - 0x380);
    assert_eq!((size(UcodeId::ImuI), size(UcodeId::ImuD)), (0x1000, 0x800));
    assert_eq!((size(UcodeId::SdmaTh0), size(UcodeId::SdmaTh1)), (0x800, 0x700));
    assert_eq!((size(UcodeId::CpMes), size(UcodeId::CpMesData)), (0x1000, 0x800));
    // IMU's DRAM part follows its IRAM part (Linux ignores the offset fields).
    let d = list.iter().find(|u| u.id == UcodeId::ImuD).unwrap();
    assert_eq!(d.range.start, 256 + 0x1000);
    // Every synthetic payload's CRC matches (the real files' mostly do not; see real_files).
    assert!(s.files.iter().all(|f| f.crc_matches()));
    // GX-7k: MES's KIQ is read and loaded (Linux forces enable_mes_kiq on GC 11.0.x), with its uc start; the TA
    // has its three applications, the TOC's payload is whole.
    assert!(s.get(Kind::MesKiq).is_some());
    assert_eq!(s.mes_uc_starts(), [Some(crate::gpucore::mesfake::UC_START); 2]);
    let ta = s.get(Kind::Ta).unwrap().ta_bins().unwrap();
    assert_eq!(ta.iter().map(|b| (b.ta_type, b.range.len())).collect::<Vec<_>>(), [(1, 0x400), (4, 0x200), (5, 0x100)]);
    assert_eq!(s.get(Kind::Toc).unwrap().payload().len(), 0x900);
}

#[test]
fn options_change_the_list_as_linux_flags_do() {
    let all = ListOpts { autoload: false, mes_kiq: true, keep_display_firmware: false, vcn: true };
    let s = set_with(&all, synth()).unwrap();
    let ids: Vec<_> = s.load_list(&all).unwrap().iter().map(|u| u.id).collect();
    for id in [UcodeId::CpMec1Jt, UcodeId::CpMes1, UcodeId::CpMes1Data, UcodeId::Vcn, UcodeId::Dmcub] {
        assert!(ids.contains(&id), "{id:?} missing: {ids:?}");
    }
    assert_eq!(ids.last(), Some(&UcodeId::Dmcub));
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "not in ID order: {ids:?}");
}

#[test]
fn rs64_mode_from_the_pfp_header() {
    let mut m = synth();
    for (name, seed) in [("gc_11_0_1_pfp", 0u32), ("gc_11_0_1_me", 1), ("gc_11_0_1_mec", 2)] {
        let b = m.get_mut(&path(name)).unwrap();
        b[8..12].copy_from_slice(&[2, 0, 0, 0]);
        put(b, 4, 60);
        put(b, 36, 0x1000); // ucode_size_bytes
        put(b, 44, 0x200 + seed * 0x10); // data size
        put(b, 48, 256 + 0x1000); // data offset
    }
    let opts = ListOpts::PSP13_0_4;
    let s = set_with(&opts, m.clone()).unwrap();
    assert!(s.rs64());
    let ids: Vec<_> = s.load_list(&opts).unwrap().iter().map(|u| u.id).collect();
    assert!(ids.contains(&UcodeId::Rs64Pfp) && ids.contains(&UcodeId::Rs64MecP3Stack) && !ids.contains(&UcodeId::CpPfp));
    // PFP says RS64, ME does not: refused, not read with the wrong layout.
    let b = m.get_mut(&path("gc_11_0_1_me")).unwrap();
    b[8..12].copy_from_slice(&[1, 0, 0, 0]);
    let s = set_with(&opts, m).unwrap();
    assert_eq!(s.load_list(&opts), Err(Why::Rs64Mixed(Kind::Me)));
}

#[test]
fn common_header_refusals() {
    let good = synth()[&path("vcn_4_0_2")].clone();
    assert!(file(Kind::Vcn, good.clone()).is_ok());
    assert_eq!(file(Kind::Vcn, good[..31].to_vec()).unwrap_err(), Why::Short { have: 31 });
    let mut b = good.clone();
    put(&mut b, 0, 99);
    assert!(matches!(file(Kind::Vcn, b).unwrap_err(), Why::Size { header: 99, .. }));
    let mut b = good.clone();
    b.push(0);
    assert!(matches!(file(Kind::Vcn, b).unwrap_err(), Why::Size { .. }), "a longer file than its header says");
    for hs in [0u32, 31, u32::MAX] {
        let mut b = good.clone();
        put(&mut b, 4, hs);
        assert!(matches!(file(Kind::Vcn, b).unwrap_err(), Why::HeaderSize { .. }), "header size {hs}");
    }
    let mut b = good.clone();
    put(&mut b, 24, u32::MAX);
    assert!(matches!(file(Kind::Vcn, b).unwrap_err(), Why::Range { what: "ucode array", .. }));
}

#[test]
fn per_kind_refusals() {
    let s = synth();
    let get = |n: &str| s[&path(n)].clone();
    let entries = |k, b| file(k, b).and_then(|f| f.entries(false));
    // RLC v1 is not read by Linux (amdgpu_rlc.c:525).
    let mut b = get("gc_11_0_1_rlc");
    b[8..10].copy_from_slice(&1u16.to_le_bytes());
    assert!(matches!(entries(Kind::Rlc, b), Err(Why::Version { kind: Kind::Rlc, major: 1, .. })));
    // RLC v2.3 whose header ends before RLC_P's fields.
    let mut b = get("gc_11_0_1_rlc");
    put(&mut b, 4, 172);
    assert!(matches!(entries(Kind::Rlc, b), Err(Why::HeaderSize { header: 172, need: 184 })));
    // A part past the file, for each kind with its own fields.
    for (n, k, at) in [
        ("gc_11_0_1_rlc", Kind::Rlc, 160usize),
        ("gc_11_0_1_imu", Kind::Imu, 40),
        ("gc_11_0_1_mes_2", Kind::MesSched, 52),
        ("sdma_6_0_1", Kind::Sdma, 48),
        ("dcn_3_1_4_dmcub", Kind::Dmcub, 32),
        ("gc_11_0_1_mec", Kind::Mec, 36),
    ] {
        let mut b = get(n);
        put(&mut b, at, 0x00F0_0000);
        assert!(matches!(entries(k, b), Err(Why::Range { .. })), "{n} field {at}");
    }
    // MEC jump table larger than its microcode.
    let mut b = get("gc_11_0_1_mec");
    put(&mut b, 40, 0x10000);
    assert!(matches!(entries(Kind::Mec, b), Err(Why::JumpTable { .. })));
    // SDMA v1 and an IMU v2 are not this GPU's formats.
    let mut b = get("sdma_6_0_1");
    b[8..10].copy_from_slice(&1u16.to_le_bytes());
    assert!(matches!(entries(Kind::Sdma, b), Err(Why::Version { kind: Kind::Sdma, .. })));
    // TA descriptors running into the microcode array.
    let mut b = get("psp_13_0_4_ta");
    put(&mut b, 32, 14);
    assert_eq!(file(Kind::Ta, b).unwrap().ta_bins(), Err(Why::TaCount(14)));
    let mut b = get("psp_13_0_4_ta");
    put(&mut b, 36 + 12, 0x0100_0000);
    assert!(matches!(file(Kind::Ta, b).unwrap().ta_bins(), Err(Why::Range { what: "ta bin", .. })));
    // Two files giving the same microcode.
    let pfp = file(Kind::Pfp, get("gc_11_0_1_pfp")).unwrap();
    let set = FwSet { files: std::vec![pfp.clone(), pfp] };
    assert_eq!(set.load_list(&ListOpts::PSP13_0_4), Err(Why::Duplicate(UcodeId::CpPfp)));
}

#[test]
fn random_damage_never_panics() {
    let mut x = 0x2545_f491_4f6c_dd1du64;
    let mut rnd = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let kinds = [
        ("gc_11_0_1_pfp", Kind::Pfp),
        ("gc_11_0_1_mec", Kind::Mec),
        ("gc_11_0_1_rlc", Kind::Rlc),
        ("gc_11_0_1_imu", Kind::Imu),
        ("gc_11_0_1_mes_2", Kind::MesSched),
        ("sdma_6_0_1", Kind::Sdma),
        ("dcn_3_1_4_dmcub", Kind::Dmcub),
        ("psp_13_0_4_ta", Kind::Ta),
    ];
    let s = synth();
    for (n, k) in kinds {
        let good = s[&path(n)].clone();
        for _ in 0..500 {
            let mut b = good.clone();
            for _ in 0..1 + rnd() % 4 {
                let at = (rnd() % 256) as usize & !3;
                put(&mut b, at, rnd() as u32);
            }
            if rnd() % 8 == 0 {
                b.truncate((rnd() % b.len() as u64) as usize);
            }
            if let Ok(f) = file(k, b) {
                for rs64 in [false, true] {
                    if let Ok(v) = f.entries(rs64) {
                        assert!(v.iter().all(|(_, r)| r.end <= f.bytes.len()));
                    }
                }
                let _ = f.ta_bins();
            }
        }
    }
}

/// A source that counts reads (to see what the loader asks for).
struct Counting(MapSource, Vec<String>);

impl FwSource for Counting {
    fn read(&mut self, path: &str) -> Result<Option<Vec<u8>>, LoadError> {
        self.1.push(String::from(path));
        self.0.read(path)
    }
}

#[test]
fn loader_fallback_missing_compressed_bad() {
    // The KIQ switch off (a pre-GX-7k list): its file is not even read.
    let opts = ListOpts { mes_kiq: false, ..ListOpts::PSP13_0_4 };
    let needs = pspfake::plan_780m().files();
    // MES `_mes_2` missing: `_mes` is taken and noted.
    let mut m = synth();
    let mes = m.remove(&path("gc_11_0_1_mes_2")).unwrap();
    m.insert(path("gc_11_0_1_mes"), mes);
    let mut src = Counting(MapSource(m.clone()), Vec::new());
    let (set, notes) = fwload::load(&needs, &mut src, &opts).unwrap();
    assert_eq!(set.get(Kind::MesSched).unwrap().name, "amdgpu/gc_11_0_1_mes.bin");
    assert_eq!(notes.fallbacks, [(String::from("amdgpu/gc_11_0_1_mes_2.bin"), String::from("amdgpu/gc_11_0_1_mes.bin"))]);
    assert!(!src.1.iter().any(|p| p.contains("mes1")), "KIQ file read although off: {:?}", src.1);
    assert!(notes.skipped.iter().any(|(n, _)| n.contains("mes1")));
    // Missing without fallback.
    let mut m2 = m.clone();
    m2.remove(&path("vcn_4_0_2"));
    assert_eq!(set_with(&opts, m2.clone()).unwrap_err(), LoadError::Missing(String::from("amdgpu/vcn_4_0_2.bin")));
    // Only the compressed file: named as such.
    m2.insert(std::format!("{}.zst", path("vcn_4_0_2")), std::vec![0x28, 0xb5, 0x2f, 0xfd]);
    assert!(matches!(set_with(&opts, m2).unwrap_err(), LoadError::Compressed(p) if p.ends_with("vcn_4_0_2.bin.zst")));
    // A damaged file: refused with its name and reason.
    let mut m3 = m;
    m3.get_mut(&path("sdma_6_0_1")).unwrap().truncate(100);
    assert!(matches!(set_with(&opts, m3).unwrap_err(), LoadError::Bad { name, why: Why::Size { .. } } if name.contains("sdma")));
}

#[test]
fn plan_files_match_the_real_table() {
    use super::blob::{Blob, build, minipc_ips};
    let d = crate::gpucore::disc::parse(&build(&Blob::new(minipc_ips())).0).unwrap();
    assert_eq!(ipmod::plan(&d).files(), pspfake::plan_780m().files());
    let names: Vec<_> = pspfake::plan_780m().files().into_iter().map(|f| f.name).collect();
    assert_eq!(names.len(), 12);
    assert!(names.iter().all(|n| synth().contains_key(&stick_path(n)) || n.ends_with("mes1.bin") || n.ends_with("_mes_2.bin")));
}

#[test]
fn crc32_is_zlibs() {
    assert_eq!(ucode::crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(ucode::crc32(b""), 0);
}

/// The host's linux-firmware-amdgpu files (CachyOS: /usr/lib/firmware/amdgpu/*.bin.zst), decompressed with the
/// host's zstd: they parse, give the 780M list in Linux's order and run through the fake. Skipped (passes)
/// where the package or zstd is missing, with a line on stderr (`--nocapture` shows it); `V2_FW_STRICT=1`
/// turns the skip into a failure, as for xtask image. Never copied anywhere.
fn skip_real(why: &str) {
    let strict = std::env::var("V2_FW_STRICT").is_ok_and(|v| v == "1");
    assert!(!strict, "real_files: {why} (V2_FW_STRICT=1)");
    std::eprintln!("real_files: SKIPPED, {why}");
}

#[test]
fn real_files() {
    let dir = std::path::Path::new("/usr/lib/firmware/amdgpu");
    let needs = pspfake::plan_780m().files();
    let mut m = BTreeMap::new();
    for n in &needs {
        for name in std::iter::once(&n.name).chain(n.fallback.as_ref()) {
            let base = name.trim_start_matches("amdgpu/");
            let zst = dir.join(std::format!("{base}.zst"));
            if !zst.exists() {
                continue;
            }
            let Ok(out) = std::process::Command::new("zstd").arg("-dcf").arg(&zst).output() else {
                skip_real("no zstd on this host");
                return;
            };
            if out.status.success() {
                m.insert(stick_path(name), out.stdout);
            }
        }
    }
    if !m.contains_key(&stick_path("amdgpu/psp_13_0_4_toc.bin")) {
        skip_real("linux-firmware-amdgpu not installed in /usr/lib/firmware/amdgpu");
        return;
    }
    let opts = ListOpts::PSP13_0_4;
    let s = set_with(&opts, m.clone()).unwrap();
    let list = s.load_list(&opts).unwrap();
    assert_eq!(list.iter().map(|u| u.id).collect::<Vec<_>>(), EXPECT_780M);
    assert!(!s.rs64(), "gc_11_0_1 CP is not RS64 (PFP header 1.0)");
    assert_eq!(s.get(Kind::Ta).unwrap().ta_bins().unwrap().len(), 3);
    for u in &list {
        std::eprintln!("{:?} type {} size {}", u.id, u.id.fw_type(), u.size());
    }
    let crc_ok: Vec<_> = s.files.iter().filter(|f| f.crc_matches()).map(|f| f.name.clone()).collect();
    std::eprintln!("crc32 matches the payload for: {crc_ok:?}");
    crate::ktests_psp::check_run(&mut pspfake::FakeGpu::new(pspfake::Hostile::None), &mut MapSource(m)).unwrap();
}
