use crate::ipfacts::*;
use std::vec::Vec;

#[test]
fn the_table_has_the_46_entries_the_card_counts() {
    // `num_ips` read 46 in sysfs.
    assert_eq!(TABLE.len(), 46);
    for pair in TABLE.windows(2) {
        assert!((pair[0].hw_id, pair[0].instance) < (pair[1].hw_id, pair[1].instance), "{pair:?}");
    }
}

#[test]
fn the_versions_linux_acts_on_are_there() {
    let version = |id| find(&TABLE, id, 0).unwrap().version;
    assert_eq!(version(hw::GC), v(12, 0, 1));
    assert_eq!(version(hw::SDMA0), v(7, 0, 1));
    assert_eq!(version(hw::SDMA1), v(7, 0, 1));
    assert_eq!(version(hw::LSDMA), v(7, 0, 1));
    assert_eq!(version(hw::MP0), v(14, 0, 3));
    assert_eq!(version(hw::MP1), v(14, 0, 3));
    assert_eq!(version(hw::OSSSYS), v(7, 0, 0));
    assert_eq!(version(hw::HDP), v(7, 0, 0));
    assert_eq!(version(hw::MMHUB), v(4, 1, 0));
    assert_eq!(version(hw::ATHUB), v(4, 1, 0));
    assert_eq!(version(hw::NBIF), v(6, 3, 1));
    assert_eq!(version(hw::DMU), v(4, 1, 0));
    assert_eq!(version(hw::UVD), v(5, 0, 0));
    assert_eq!(version(hw::SMUIO), v(14, 0, 2));
    assert_eq!(version(hw::DF), v(4, 15, 1));
}

#[test]
fn instances_count_as_the_note_says() {
    let count = |id| TABLE.iter().filter(|e| e.hw_id == id).count();
    assert_eq!(count(hw::CLKA), 9);
    assert_eq!(count(hw::UMC), 8);
    assert!(find(&TABLE, hw::UMC, 7).is_some());
    assert!(find(&TABLE, hw::UMC, 8).is_none());
}

#[test]
fn register_bases_are_the_measured_ones() {
    let bases = |id| find(&TABLE, id, 0).unwrap().bases;
    assert_eq!(bases(hw::GC), &[0x1260, 0xA000, 0x1C000, 0x0240_2C00]);
    assert_eq!(bases(hw::GC), bases(hw::SDMA0));
    assert_eq!(bases(hw::MP0), bases(hw::MP1));
    assert_eq!(bases(hw::MP0)[..3], [0x16000, 0x16200, 0x1CE00]);
    assert_eq!(bases(hw::NBIF), &[0, 0x14, 0xD20, 0x10400, 0x0241_B000, 0x0404_0000]);
    assert_eq!(bases(hw::OSSSYS), &[0x10A0, 0x0240_A000]);
    assert_eq!(bases(hw::MMHUB), &[0x1A000, 0x0240_8800]);
    assert_eq!(bases(hw::DMU), &[0x12, 0xC0, 0x34C0, 0x9000, 0x0240_3C00]);
    assert_eq!(bases(hw::PWR), &[] as &[u32]);
}

/// The same twelve as `rdna4_reports_exactly_the_missing_modules` in `crates/drivers/amdgpu/src/tests/ipmod.rs`, whose
/// fixture was synthetic: the measured table agrees with it.
#[test]
fn with_no_module_built_all_twelve_roles_are_missing() {
    let missing: Vec<(Role, IpVersion)> = missing_roles(&TABLE, &[]).map(|m| (m.role, m.version)).collect();
    let want = [
        (Role::Psp, v(14, 0, 3)),
        (Role::Smu, v(14, 0, 3)),
        (Role::Nbio, v(6, 3, 1)),
        (Role::Gmc, v(12, 0, 1)),
        (Role::Gfxhub, v(12, 0, 1)),
        (Role::Mmhub, v(4, 1, 0)),
        (Role::Ih, v(7, 0, 0)),
        (Role::Gfx, v(12, 0, 1)),
        (Role::Mes, v(12, 0, 1)),
        (Role::Sdma, v(7, 0, 1)),
        (Role::Display, v(4, 1, 0)),
        (Role::Vcn, v(5, 0, 0)),
    ];
    assert_eq!(missing, want);
}

#[test]
fn a_neighbouring_version_does_not_serve_a_role() {
    let built = [
        Module { role: Role::Gfx, version: v(12, 0, 0) },
        Module { role: Role::Sdma, version: v(6, 0, 1) },
        Module { role: Role::Psp, version: v(13, 0, 4) },
    ];
    assert_eq!(missing_roles(&TABLE, &built).count(), 12);
}

#[test]
fn a_built_module_takes_only_its_own_role_off_the_list() {
    let built = [Module { role: Role::Gfx, version: v(12, 0, 1) }];
    let missing: Vec<Role> = missing_roles(&TABLE, &built).map(|m| m.role).collect();
    assert_eq!(missing.len(), 11);
    assert!(!missing.contains(&Role::Gfx));
    assert!(missing.contains(&Role::Mes), "MES keys on GC too and has its own module");
}

#[test]
fn an_ip_the_table_lacks_needs_no_module() {
    let headless: Vec<Entry> = TABLE.iter().copied().filter(|e| e.hw_id != hw::DMU).collect();
    assert!(missing_roles(&headless, &[]).all(|m| m.role != Role::Display));
    assert_eq!(missing_roles(&headless, &[]).count(), 11);
}

#[test]
fn the_display_runs_under_the_version_linux_renames_it_to() {
    assert_eq!(display_version(find(&TABLE, hw::DMU, 0).unwrap().version), v(4, 0, 1));
    assert_eq!(display_version(v(3, 1, 4)), v(3, 1, 4));
    assert_eq!(std::format!("{}", v(12, 0, 1)), "12.0.1");
}

#[test]
fn every_role_names_a_linux_block() {
    for role in Role::ALL {
        assert!(!role.linux_block().is_empty());
    }
    assert_eq!(LINUX_BLOCKS.len(), 11);
    // Secure processor before SMU, both before graphics; MES last.
    let at = |name| LINUX_BLOCKS.iter().position(|b| *b == name).unwrap();
    assert!(at("psp") < at("smu") && at("smu") < at("gfx_v12_0"));
    assert_eq!(at("mes_v12_0"), LINUX_BLOCKS.len() - 1);
}
