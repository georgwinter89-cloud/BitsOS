use crate::fwset::*;
use std::vec::Vec;

const NAVI48: u16 = 0x7550;
const THIS_REV: u8 = 0xC0;

#[test]
fn this_board_is_not_a_kicker_board() {
    assert!(!is_kicker(NAVI48, THIS_REV));
    assert!(is_kicker(0x744B, 0x00));
    assert!(is_kicker(0x7551, 0xC8));
    // Device and revision count together.
    assert!(!is_kicker(0x7551, 0xC0));
    assert!(!is_kicker(NAVI48, 0xC8));
    assert!(!is_kicker(0x744B, 0xC8));
}

#[test]
fn the_load_set_of_this_board_is_the_roadmaps_list() {
    let files: Vec<&str> = load_set(NAVI48, THIS_REV, Mes::Unified).map(|c| c.file).collect();
    assert_eq!(
        files,
        [
            "amdgpu/psp_14_0_3_sos.bin",
            "amdgpu/psp_14_0_3_ta.bin",
            "amdgpu/smu_14_0_3.bin",
            "amdgpu/gc_12_0_1_pfp.bin",
            "amdgpu/gc_12_0_1_me.bin",
            "amdgpu/gc_12_0_1_mec.bin",
            "amdgpu/gc_12_0_1_rlc.bin",
            "amdgpu/gc_12_0_1_imu.bin",
            "amdgpu/gc_12_0_1_uni_mes.bin",
            "amdgpu/sdma_7_0_1.bin",
            "amdgpu/dcn_4_0_1_dmcub.bin",
            "amdgpu/vcn_5_0_0.bin",
        ]
    );
}

#[test]
fn the_load_set_adds_up_to_the_roadmaps_total() {
    let total: u32 = load_set(NAVI48, THIS_REV, Mes::Unified).map(|c| c.expected.unwrap().0).sum();
    assert_eq!(total, 2_749_808 + 435_632);
}

#[test]
fn the_secure_os_file_is_in_every_load_set() {
    // The card starts cold without a secure OS (the trace of 18.11.): the driver loads it, with the bootloader's
    // parts, the TOC and the register list of the same file.
    for mes in [Mes::Unified, Mes::TwoFiles] {
        for (device, revision) in [(NAVI48, THIS_REV), (0x7551, 0xC8)] {
            assert_eq!(load_set(device, revision, mes).filter(|c| c.file.contains("_sos")).count(), 1, "{device:#x} {mes:?}");
        }
    }
    let sos = FILES.iter().find(|f| f.file.contains("_sos")).unwrap();
    assert_eq!(sos.usage, Use::Load);
    assert_eq!(sos.bytes, 435_632);
}

#[test]
fn the_table_of_contents_is_only_for_a_load_type_we_do_not_use() {
    assert!(load_set(NAVI48, THIS_REV, Mes::Unified).all(|c| !c.file.contains("_toc")));
    assert_eq!(FILES.iter().find(|f| f.file.contains("_toc")).unwrap().usage, Use::NotNeeded);
}

#[test]
fn mes_is_one_file_or_two_never_both() {
    let mes = |choice| -> Vec<&str> {
        load_set(NAVI48, THIS_REV, choice).map(|c| c.file).filter(|f| f.contains("_mes")).collect()
    };
    assert_eq!(mes(Mes::Unified), ["amdgpu/gc_12_0_1_uni_mes.bin"]);
    assert_eq!(mes(Mes::TwoFiles), ["amdgpu/gc_12_0_1_mes.bin", "amdgpu/gc_12_0_1_mes1.bin"]);
}

#[test]
fn a_kicker_board_gets_other_names_and_no_promised_hash() {
    let set: Vec<Chosen> = load_set(0x7551, 0xC8, Mes::Unified).collect();
    let renamed: Vec<&str> = set.iter().filter(|c| c.expected.is_none()).map(|c| c.file).collect();
    assert_eq!(
        renamed,
        [
            "amdgpu/psp_14_0_3_sos_kicker.bin",
            "amdgpu/psp_14_0_3_ta_kicker.bin",
            "amdgpu/smu_14_0_3_kicker.bin",
            "amdgpu/gc_12_0_1_rlc_kicker.bin",
            "amdgpu/gc_12_0_1_imu_kicker.bin",
        ]
    );
    assert!(set.iter().any(|c| c.file == "amdgpu/gc_12_0_1_pfp.bin" && c.expected.is_some()));
    assert_eq!(set.len(), 12);
}

#[test]
fn every_hash_is_64_hex_digits_and_no_two_files_are_the_same() {
    for f in FILES {
        assert_eq!(f.sha256.len(), 64, "{}", f.file);
        assert!(f.sha256.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()), "{}", f.file);
        assert!(f.bytes > 0, "{}", f.file);
        assert!(f.file.starts_with("amdgpu/") && f.file.ends_with(".bin"), "{}", f.file);
    }
    for (i, a) in FILES.iter().enumerate() {
        for b in &FILES[i + 1..] {
            assert_ne!(a.sha256, b.sha256, "{} {}", a.file, b.file);
            assert_ne!(a.file, b.file);
        }
    }
}
