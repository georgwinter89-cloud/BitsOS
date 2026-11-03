//! Encoders against the bit values of linux-6.6 amdgpu_vm.h:57-112 and gmc_v11_0.c:446-545.

use crate::pte::*;
use crate::store::TableAddr;

#[test]
fn bit_positions_match_linux() {
    assert_eq!(VALID, 0x1);
    assert_eq!(SYSTEM, 0x2);
    assert_eq!(SNOOPED, 0x4);
    assert_eq!(TMZ, 0x8);
    assert_eq!(EXECUTABLE, 0x10);
    assert_eq!(READABLE, 0x20);
    assert_eq!(WRITEABLE, 0x40);
    assert_eq!(FRAG_MASK, 0xf80);
    assert_eq!(MTYPE_MASK, 0x0007_0000_0000_0000);
    assert_eq!(PRT, 1 << 51);
    assert_eq!(PDE_PTE, 1 << 54);
    assert_eq!(LOG, 1 << 55);
    assert_eq!(TF, 1 << 56);
    assert_eq!(NOALLOC, 1 << 58);
    assert_eq!(BFS_MASK, 0xf800_0000_0000_0000);
    assert_eq!(INVALID_PTE, 0x10);
    assert_eq!(INVALID_PDE, 0x0040_0000_0000_0000);
}

#[test]
fn system_page_rw_snooped() {
    let e = leaf(0x1234_5000, 4096, Target::System { snooped: true }, MapFlags::RW, 0, false).unwrap();
    assert_eq!(e, 0x1234_5067);
    assert_eq!(
        decode(e, true).unwrap(),
        Entry::Page {
            phys: 0x1234_5000,
            target: Target::System { snooped: true },
            flags: MapFlags::RW,
            frag: 0,
            pde_as_pte: false
        }
    );
}

#[test]
fn carveout_huge_leaf_with_mtype_and_frag() {
    let f = MapFlags { exec: true, mtype: Mtype::Uc, ..MapFlags::RW };
    let e = leaf(0x6_6020_0000, 2 << 20, Target::Local, f, 9, true).unwrap();
    assert_eq!(e, 0x6_6020_0000 | 0x71 | (9 << 7) | (3 << 48) | (1 << 54));
    // Only aligned to 4 KiB, not 2 MiB: refused for a huge leaf.
    assert_eq!(leaf(0x6_6020_1000, 2 << 20, Target::Local, f, 9, true), Err(EncodeError::Misaligned(0x6_6020_1000)));
    assert_eq!(leaf(1 << 48, 4096, Target::Local, f, 0, false), Err(EncodeError::TooWide(1 << 48)));
    assert_eq!(leaf(0x1000, 4096, Target::Local, f, 32, false), Err(EncodeError::Frag(32)));
}

#[test]
fn prt_entry_like_gmc_v11_get_vm_pte() {
    let e = prt(MapFlags::default(), 0, false).unwrap();
    assert_eq!(e, PRT | SNOOPED | LOG | SYSTEM);
    assert_eq!(e & VALID, 0);
    assert_eq!(decode(e, true).unwrap(), Entry::Prt { frag: 0 });
}

#[test]
fn pde_address_and_flags() {
    let t = TableAddr { phys: 0x80_1FD0_0000, target: Target::Local };
    assert_eq!(pde(t).unwrap(), 0x80_1FD0_0001);
    let s = TableAddr { phys: 0x1_0000_0000, target: Target::System { snooped: true } };
    assert_eq!(pde(s).unwrap(), 0x1_0000_0007);
    assert_eq!(decode(0x1_0000_0007, false).unwrap(), Entry::Table(s));
    // Low 6 bits set: gmc_v11_0.c:500 BUG_ON in Linux, an error here.
    let bad = TableAddr { phys: 0x1_0000_0020, target: Target::Local };
    assert_eq!(pde(bad), Err(EncodeError::Misaligned(0x1_0000_0020)));
    let wide = TableAddr { phys: 1 << 48, target: Target::Local };
    assert_eq!(pde(wide), Err(EncodeError::TooWide(1 << 48)));
    assert_eq!(pd_addr(t).unwrap(), 0x80_1FD0_0001);
}

#[test]
fn decode_refuses_what_gr_never_writes() {
    let page = 0x1000 | VALID;
    assert!(decode(page | TF, true).is_err());
    assert!(decode(page | BFS_MASK, false).is_err(), "BFS on a pointer");
    assert!(decode(page | (5 << 48), true).is_err(), "mtype 5");
    assert!(decode(page | PDE_PTE, true).is_err(), "PDE_PTE on a PTB entry");
    assert!(decode(page | LOG, true).is_err());
    assert!(decode(page | SNOOPED, true).is_err(), "snooped carve-out page");
    assert!(decode(page | TMZ | READABLE, true).is_ok(), "TMZ is fine");
    assert!(decode(0x1000 | VALID | 0x8, false).is_err(), "reserved PDE bit 3");
    assert_eq!(decode(INVALID_PTE, true).unwrap(), Entry::Invalid);
    assert_eq!(decode(INVALID_PDE, false).unwrap(), Entry::Invalid);
}

#[test]
fn uapi_flags_map_like_amdgpu_gem_va_map_flags() {
    let (f, prt) = MapFlags::from_uapi(uapi::PAGE_READABLE | uapi::PAGE_WRITEABLE | uapi::MTYPE_UC);
    assert!(!prt);
    assert_eq!(f, MapFlags { mtype: Mtype::Uc, ..MapFlags::RW });
    // RW (5 << 5) and DEFAULT map to NC on GFX 11.
    assert_eq!(MapFlags::from_uapi(5 << 5).0.mtype, Mtype::Nc);
    assert!(MapFlags::from_uapi(uapi::PAGE_PRT | uapi::PAGE_EXECUTABLE).0.exec);
    assert!(MapFlags::from_uapi(uapi::PAGE_PRT).1);
    assert!(MapFlags::from_uapi(uapi::PAGE_NOALLOC).0.noalloc);
}

#[test]
fn random_entries_never_panic() {
    let mut rng = super::Rng::new(7);
    for _ in 0..200_000 {
        let e = rng.next();
        let _ = decode(e, true);
        let _ = decode(e, false);
    }
}

#[test]
fn prt_bits_without_linux_companions_are_refused() {
    // A directory entry without PDE_PTE is a pointer to the hardware, never a PRT range.
    let dir = PRT | SNOOPED | LOG | SYSTEM;
    assert_eq!(decode(dir, false), Err(DecodeError::Reserved(dir)));
    assert_eq!(decode(dir | PDE_PTE, false).unwrap(), Entry::Prt { frag: 0 });
    for missing in [SNOOPED, LOG, SYSTEM] {
        let e = dir & !missing;
        assert_eq!(decode(e, true), Err(DecodeError::Reserved(e)), "{missing:#x}");
    }
}
