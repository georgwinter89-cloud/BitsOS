//! Host tests of Reload's pure parts: ELF stream, URLs, memory map, page tables, BootInfo hand-over.

extern crate std;

use crate::elf::{ElfError, ElfStream, ImageSink, Layout};
use crate::handover::{self, NewBoot};
use crate::memmap::{self, Mark, MapError};
use crate::tables::{Perm, Tables, estimate};
use crate::url::{self, UrlError};
use crate::{Fnv, testkit};
use bootinfo::layout::{DIRECT_MAP_BASE, KERNEL_BASE, VMAP_BASE};
use bootinfo::*;
use std::string::String;
use std::vec;
use std::vec::Vec;

const MB: u64 = 1 << 20;

struct Collect(Vec<u8>);
impl ImageSink for Collect {
    fn layout(&mut self, l: &Layout) -> Result<(), String> {
        self.0 = vec![0; l.span as usize];
        Ok(())
    }
    fn write(&mut self, off: u64, d: &[u8]) -> Result<(), String> {
        self.0[off as usize..off as usize + d.len()].copy_from_slice(d);
        Ok(())
    }
}

fn load(file: &[u8], piece: usize) -> Result<(Layout, Vec<u8>), ElfError> {
    let mut s = ElfStream::new();
    let mut c = Collect(Vec::new());
    for p in file.chunks(piece) {
        s.feed(p, &mut c)?;
    }
    Ok((s.finish()?, c.0))
}

#[test]
fn elf_streams_in_any_piece_size() {
    let k = testkit::small_kernel();
    let (l, img) = load(&k, k.len()).unwrap();
    assert_eq!((l.lo, l.span, l.entry), (KERNEL_BASE, 0x6000, KERNEL_BASE + 0x10));
    assert_eq!(&img[..4], &[1, 8, 15, 22]);
    assert_eq!(img[0x2200], 0xA0);
    assert!(img[0x2200 + 100..].iter().all(|&b| b == 0), "bss zero");
    for p in [1, 3, 63, 64, 65, 4095, 4097, 65536] {
        assert_eq!(load(&k, p).unwrap().1, img, "piece {p}");
    }
}

#[test]
fn elf_refusals() {
    let k = testkit::small_kernel();
    let with = |off: usize, bytes: &[u8]| {
        let mut b = k.clone();
        b[off..off + bytes.len()].copy_from_slice(bytes);
        load(&b, 100).unwrap_err()
    };
    assert_eq!(with(1, b"X"), ElfError::NotElf);
    assert_eq!(with(4, &[1]), ElfError::Not64BitLittleEndian);
    assert_eq!(with(18, &[0x28, 0]), ElfError::NotX86_64);
    assert_eq!(with(16, &[3, 0]), ElfError::NotExecutable);
    assert_eq!(with(54, &[32, 0]), ElfError::BadHeaderSize);
    assert!(matches!(with(32, &70_000u64.to_le_bytes()), ElfError::HeadersTooFar { .. }));
    assert!(matches!(with(24, &(KERNEL_BASE + 0x2000).to_le_bytes()), ElfError::EntryNotExecutable { .. }));
    // filesz > memsz in the first program header.
    assert_eq!(with(64 + 40, &1u64.to_le_bytes()), ElfError::FileBytesOverMemory);
    assert!(matches!(load(&k[..0x4000 + 99], 512).unwrap_err(), ElfError::Truncated { have: 0x4063, need: 0x4064 }));
    assert_eq!(load(&[], 1).unwrap_err(), ElfError::NotElf);
    assert!(matches!(load(&k[..40], 8).unwrap_err(), ElfError::Truncated { .. }));
    let low = testkit::elf(&[testkit::Seg { vaddr: 0x1000, flags: 5, data: &[1; 8], memsz: 8 }], 0x1000);
    assert!(matches!(load(&low, 64).unwrap_err(), ElfError::BelowKernelBase { vaddr: 0x1000 }));
    let big = testkit::elf(&[testkit::Seg { vaddr: KERNEL_BASE, flags: 5, data: &[1; 8], memsz: (1 << 30) + 1 }], KERNEL_BASE);
    assert!(matches!(load(&big, 64).unwrap_err(), ElfError::SpanTooLarge { .. }));
    let wrap = testkit::elf(&[testkit::Seg { vaddr: u64::MAX - 0xFFF, flags: 5, data: &[1; 8], memsz: 0x2000 }], u64::MAX - 0xFFF);
    assert_eq!(load(&wrap, 64).unwrap_err(), ElfError::Overflow);
    let overlap = testkit::elf(
        &[
            testkit::Seg { vaddr: KERNEL_BASE, flags: 5, data: &[1; 16], memsz: 0x100 },
            testkit::Seg { vaddr: KERNEL_BASE + 0x80, flags: 6, data: &[2; 16], memsz: 0x100 },
        ],
        KERNEL_BASE,
    );
    assert!(matches!(load(&overlap, 64).unwrap_err(), ElfError::SegmentsOverlap { .. }));
    let none = testkit::elf(&[], KERNEL_BASE);
    assert_eq!(load(&none, 64).unwrap_err(), ElfError::NoLoadSegments);
    // The sink refusing (no memory) ends the stream.
    struct No;
    impl ImageSink for No {
        fn layout(&mut self, _: &Layout) -> Result<(), String> {
            Err("does not fit".into())
        }
        fn write(&mut self, _: u64, _: &[u8]) -> Result<(), String> {
            Ok(())
        }
    }
    assert_eq!(ElfStream::new().feed(&k, &mut No).unwrap_err(), ElfError::Sink("does not fit".into()));
}

#[test]
fn urls() {
    assert!(url::check("http://192.168.100.20:8770/SYS/3/").is_ok());
    assert!(url::check("http://10.0.2.2/KERNEL").is_ok());
    assert!(url::check("http://127.0.0.1:23051/k").is_ok());
    assert_eq!(url::check("https://192.168.100.20/K").unwrap_err(), UrlError::Https);
    assert_eq!(url::check("ftp://192.168.100.20/K").unwrap_err(), UrlError::Scheme("ftp".into()));
    assert_eq!(url::check("tftp://10.0.0.1/K").unwrap_err(), UrlError::Scheme("tftp".into()));
    assert_eq!(url::check("http://gaming-pc/K").unwrap_err(), UrlError::NeedsAddress);
    assert_eq!(url::check("http://1.1.1.1/K").unwrap_err(), UrlError::NotLan("1.1.1.1".into()));
    assert!(matches!(url::check("10.0.0.1/K").unwrap_err(), UrlError::Bad(_)));
    assert!(matches!(url::check("http://user@10.0.0.1/K").unwrap_err(), UrlError::Bad(_)));
    let b = url::bundle("http://10.0.2.2:23010/3/", None).unwrap();
    assert_eq!((b.kernel.as_str(), b.initrd.as_deref()), ("http://10.0.2.2:23010/3/KERNEL", Some("http://10.0.2.2:23010/3/INITRD")));
    let b = url::bundle("http://10.0.2.2/K.ELF", None).unwrap();
    assert_eq!(b.initrd, None, "a file URL alone keeps the running initramfs");
    let b = url::bundle("http://10.0.2.2/K.ELF", Some("http://10.0.2.2/I.CPIO")).unwrap();
    assert_eq!(b.initrd.as_deref(), Some("http://10.0.2.2/I.CPIO"));
    assert_eq!(url::bundle("http://10.0.2.2/K", Some("http://8.8.8.8/I")).unwrap_err(), UrlError::NotLan("8.8.8.8".into()));
    assert!(UrlError::Scheme("ftp".into()).describe().contains("wrong URL scheme"));
}

fn d(ty: u32, phys: u64, bytes: u64) -> MemDesc {
    MemDesc { ty, phys, virt: 0, pages: bytes / 4096, attr: 0xF }
}

/// Roughly the map bitsboot hands over under QEMU with 2 GiB.
fn qemu_map() -> Vec<MemDesc> {
    vec![
        d(memtype::CONVENTIONAL, 0, 0x9F000),
        d(memtype::BITS_KEEP, 0x9F000, 0x1000),
        d(memtype::CONVENTIONAL, 0x10_0000, 0x1EF0_0000),
        d(memtype::BITS_KEEP, 0x1F00_0000, 0x1000),
        d(memtype::CONVENTIONAL, 0x1F00_1000, 0x5E27_B000 - 0x1F00_1000),
        d(memtype::BITS_KERNEL, 0x7D27_C000, 0xF0000),
        d(memtype::BOOT_SERVICES_DATA, 0x7D36_C000, 0x10_0000),
        d(memtype::BITS_PAGETABLE, 0x7D46_C000, 50 * 4096),
        d(memtype::BITS_KEEP, 0x7E01_C000, 143_360),
        d(memtype::BITS_MODULE, 0x7E03_F000, 0x8F000),
        d(memtype::LOADER_DATA, 0x7E0C_E000, 0x51000),
        d(memtype::BITS_KEEP, 0x7E11_F000, 0x1000),
        d(memtype::RUNTIME_SERVICES_DATA, 0x7E12_0000, 0x10_0000),
        d(memtype::ACPI_NVS, 0x7FF0_0000, 0x10_0000),
        d(memtype::MMIO, 0xE000_0000, 0x1000_0000),
    ]
}

fn ty_at(map: &[MemDesc], p: u64) -> Option<u32> {
    map.iter().find(|d| d.phys <= p && p < d.end()).map(|d| d.ty)
}

#[test]
fn memory_map_gives_back_the_old_kernel_and_marks_the_new_one() {
    let old = qemu_map();
    let marks = [
        Mark { start: 0x4000_0000, end: 0x4010_0000, ty: memtype::BITS_KERNEL },
        Mark { start: 0x4010_0000, end: 0x4010_4000, ty: memtype::BITS_KERNEL },
        Mark { start: 0x4020_0000, end: 0x4020_8000, ty: memtype::BITS_PAGETABLE },
        Mark { start: 0x4030_0000, end: 0x4039_0000, ty: memtype::BITS_MODULE },
    ];
    let map = memmap::rebuild(old.iter().copied(), &[(0x7E03_F000, 0x7E03_F000 + 0x8E123)], &marks).unwrap();
    assert_eq!(ty_at(&map, 0x7D27_C000), Some(memtype::CONVENTIONAL), "old image free");
    assert_eq!(ty_at(&map, 0x7D46_C000), Some(memtype::CONVENTIONAL), "old tables free");
    assert_eq!(ty_at(&map, 0x7E03_F000), Some(memtype::CONVENTIONAL), "old initramfs free");
    for p in [0x9F000, 0x1F00_0000, 0x7E01_C000, 0x7E11_F000] {
        assert_eq!(ty_at(&map, p), Some(memtype::BITS_KEEP), "KEEP {p:#x}");
    }
    assert_eq!(ty_at(&map, 0x4000_0000), Some(memtype::BITS_KERNEL));
    assert_eq!(ty_at(&map, 0x4010_3FFF), Some(memtype::BITS_KERNEL));
    assert_eq!(ty_at(&map, 0x4010_4000), Some(memtype::CONVENTIONAL));
    assert_eq!(ty_at(&map, 0x4020_7000), Some(memtype::BITS_PAGETABLE));
    assert_eq!(ty_at(&map, 0x4038_F000), Some(memtype::BITS_MODULE));
    assert_eq!(ty_at(&map, 0xE000_0000), Some(memtype::MMIO));
    // Sorted, no overlaps, no empty entries, adjacent equal ones merged.
    for w in map.windows(2) {
        assert!(w[0].end() <= w[1].phys && w[0].pages > 0);
        assert!(!(w[0].ty == w[1].ty && w[0].attr == w[1].attr && w[0].end() == w[1].phys), "unmerged {:?}", w);
    }
    let (free, largest) = memmap::free_after(&map);
    assert!(free > 1500 * MB && largest > 512 * MB, "{free} {largest}");
    // Same RAM as before: the direct map does not change.
    assert_eq!(memmap::ram_ranges(&map), memmap::ram_ranges(&old));
}

#[test]
fn memory_map_does_not_grow_over_ten_reloads() {
    let mut map = qemu_map();
    let mut sizes = Vec::new();
    for i in 0..10u64 {
        let base = 0x3000_0000 + (i % 3) * 0x100_0000;
        let marks = [
            Mark { start: base, end: base + 0x10_0000, ty: memtype::BITS_KERNEL },
            Mark { start: base + 0x20_0000, end: base + 0x20_4000, ty: memtype::BITS_KERNEL },
            Mark { start: base + 0x40_0000, end: base + 0x41_0000, ty: memtype::BITS_PAGETABLE },
            Mark { start: base + 0x80_0000, end: base + 0x89_0000, ty: memtype::BITS_MODULE },
        ];
        let release: Vec<(u64, u64)> = map.iter().filter(|d| d.ty == memtype::BITS_MODULE).map(|d| (d.phys, d.end())).collect();
        map = memmap::rebuild(map.iter().copied(), &release, &marks).unwrap();
        sizes.push(map.len());
    }
    assert!(sizes.windows(2).all(|w| w[1] <= w[0] + 1), "map grows: {sizes:?}");
    assert!(*sizes.last().unwrap() <= qemu_map().len() + 8, "{sizes:?}");
}

#[test]
fn memory_map_refuses_marks_outside_free_memory() {
    let old = qemu_map();
    let keep = [Mark { start: 0x7E01_C000, end: 0x7E01_D000, ty: memtype::BITS_KERNEL }];
    assert!(matches!(memmap::rebuild(old.iter().copied(), &[], &keep), Err(MapError::NotCovered { .. })));
    let hole = [Mark { start: 0x8000_0000, end: 0x8000_1000, ty: memtype::BITS_KERNEL }];
    assert!(matches!(memmap::rebuild(old.iter().copied(), &[], &hole), Err(MapError::NotCovered { .. })));
    let odd = [Mark { start: 0x4000_0800, end: 0x4000_1000, ty: memtype::BITS_KERNEL }];
    assert!(matches!(memmap::rebuild(old.iter().copied(), &[], &odd), Err(MapError::NotAligned { .. })));
    // A module that is not a module cannot be released.
    assert!(memmap::rebuild(old.iter().copied(), &[(0x1000, 0x2000)], &[]).is_err());
}

#[test]
fn keep_coverage_for_the_stub_page_and_the_bootinfo_block() {
    let map = qemu_map();
    let keep = |s: u64, e: u64| memmap::covered_by(map.iter().copied(), s, e, memtype::BITS_KEEP);
    assert!(keep(0x7E11_F000, 0x7E12_0000), "a whole KEEP page");
    assert!(keep(0x7E01_C000, 0x7E01_C000 + 143_360), "the whole KEEP block");
    assert!(!keep(0x7E01_C000, 0x7E01_C000 + 143_360 + 1), "one byte past the block (a bad capacity field)");
    assert!(!keep(0x7E01_B000, 0x7E01_C000 + 4096), "starts before the block");
    assert!(!keep(0x4000_0000, 0x4000_1000), "free RAM is not KEEP");
    assert!(!keep(0x8000_0000, 0x8000_1000), "a hole");
    assert!(!keep(0x7E11_F000, 0x7E11_F000), "empty");
    // Adjacent KEEP descriptors cover a range together; a gap between them does not.
    let split = [d(memtype::BITS_KEEP, 0x1000, 0x1000), d(memtype::BITS_KEEP, 0x2000, 0x1000), d(memtype::BITS_KEEP, 0x4000, 0x1000)];
    assert!(memmap::covered_by(split.iter().copied(), 0x1800, 0x2800, memtype::BITS_KEEP));
    assert!(!memmap::covered_by(split.iter().copied(), 0x1800, 0x4800, memtype::BITS_KEEP));
}

fn build_tables(ram: &[(u64, u64)]) -> (Tables, u64) {
    let est = estimate(ram, 0x6000, 0x40_0000);
    let mut t = Tables::new(0x5000_0000, est as usize, true);
    let img = 0x4000_0000;
    t.map_range(KERNEL_BASE, img, 0x2000, Perm { write: false, exec: true, pat1: false }).unwrap();
    t.map_range(KERNEL_BASE + 0x2000, img + 0x2000, 0x1000, Perm { write: false, exec: false, pat1: false }).unwrap();
    t.map_range(KERNEL_BASE + 0x2000, img + 0x2000, 0x4000, Perm { write: true, exec: false, pat1: false }).unwrap();
    let top = t.map_direct(ram).unwrap();
    t.map_range(VMAP_BASE, 0x8000_0000, 0x40_0000, Perm { write: true, exec: false, pat1: true }).unwrap();
    t.map_range(0x7E11_F000, 0x7E11_F000, 0x1000, Perm { write: false, exec: true, pat1: false }).unwrap();
    assert!(t.used() as u64 <= est);
    (t, top)
}

#[test]
fn page_tables_follow_bitsboots_entry_contract() {
    let ram = memmap::ram_ranges(&qemu_map());
    let (t, top) = build_tables(&ram);
    assert_eq!(top, 0x8000_0000);
    let (p, f) = t.translate(KERNEL_BASE + 0x123).unwrap();
    assert_eq!((p, f), (0x4000_0123, Perm { write: false, exec: true, pat1: false }));
    let (_, f) = t.translate(KERNEL_BASE + 0x2000).unwrap();
    assert_eq!(f, Perm { write: true, exec: false, pat1: false }, "a shared page gets both segments' rights");
    assert_eq!(t.translate(KERNEL_BASE + 0x6000), None);
    for p in [0x1000u64, 0x9F000, 0x10_0000, 0x1F00_0000, 0x7E01_C123, 0x7FF0_0000] {
        let (q, f) = t.translate(DIRECT_MAP_BASE + p).unwrap();
        assert_eq!((q, f), (p, Perm { write: true, exec: false, pat1: false }), "direct map {p:#x}");
    }
    assert_eq!(t.translate(DIRECT_MAP_BASE + 0xE000_0000), None, "MMIO is not in the direct map");
    assert_eq!(t.translate(DIRECT_MAP_BASE + 0xA0000), None, "the hole below 1 MiB is not RAM");
    assert_eq!(t.translate(VMAP_BASE + 0x1234).unwrap(), (0x8000_1234, Perm { write: true, exec: false, pat1: true }));
    assert_eq!(t.translate(0x7E11_F000).unwrap(), (0x7E11_F000, Perm { write: false, exec: true, pat1: false }));
    assert_eq!(t.translate(0x7E12_0000), None, "only the stub page is identity-mapped");
    // 2 MiB pages where whole blocks are RAM: 2 GiB of RAM costs a handful of tables, not 1024.
    assert!(t.used() < 32, "{} table pages", t.used());
    let bytes = t.bytes();
    assert_eq!(bytes.len(), t.used() * 4096);
    let pml4_slot = u64::from_le_bytes(bytes[(511 * 8)..(512 * 8)].try_into().unwrap());
    assert_eq!(pml4_slot & 0xFFF, 3, "intermediate entries are P|W");
    assert_eq!(pml4_slot & !0xFFF & 0x000F_FFFF_FFFF_F000, 0x5000_0000 + 4096, "the kernel's PDPT is the first table after the PML4");
}

#[test]
fn page_table_estimate_holds_for_a_big_fragmented_machine() {
    // 32 GiB in 40 ranges with odd ends (the Mini-PC has ~30 RAM descriptors after merging).
    let mut ram = Vec::new();
    let mut a = 0x1000u64;
    for i in 0..40u64 {
        let len = (800 * MB) + i * 0x7000 + 0x3000;
        ram.push((a, a + len));
        a += len + 0x5000 + i * 0x1000;
    }
    let (t, _) = build_tables(&ram);
    assert!(t.used() > 1);
    let full = Tables::new(0, 1, true).map_direct(&ram);
    assert!(full.is_err(), "a one-page pool is reported full, not overrun");
}

fn old_bootinfo(buf: &mut [u8], counter: u32, extra_tag: bool) -> usize {
    let mut w = Writer::new(buf).unwrap();
    w.handover(Handover { cr0: 0x8001_0033, cr3: 0x7f80_1000, cr4: 0x668, efer: 0xd00, flags: 2 }).unwrap();
    w.generation(Generation { number: 3, reason: gen_reason::NEWEST_OK, skipped: 0, flags: 0 }).unwrap();
    w.kernel_image(KernelImage { phys_base: 0x7D27_C000, phys_size: 0xF0000, virt_base: KERNEL_BASE, entry: KERNEL_BASE }).unwrap();
    w.module(&Module { kind: module_kind::INITRAMFS, phys: 0x7E03_F000, size: 0x8E123, name: b"INITRD" }).unwrap();
    w.module(&Module { kind: module_kind::FIRMWARE, phys: 0x7E0C_E000, size: 0x1000, name: b"rtw.bin" }).unwrap();
    w.config(b"token=x\nmcp_port=8765\n").unwrap();
    if extra_tag {
        w.record(99, b"from a newer loader").unwrap();
        // BT-edid: the monitor's EDID (any bytes here; the kernel's readers check it).
        w.edid(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x1C, 0xAE]).unwrap();
    }
    w.tsc(Tsc { hint_hz: 3_900_000_000, calibrated_hz: 0, source: tsc_source::UEFI_STALL, calibrated_at_reload: 0 }).unwrap();
    w.reload(Reload { counter }).unwrap();
    w.crash_page(PhysRange { phys: 0x1F00_0000, size: 4096 }).unwrap();
    w.trampoline(Trampoline { page_phys: 0x9F000, low_pml4_phys: 0x7E11_E000 }).unwrap();
    w.reload_stub(PhysRange { phys: 0x7E11_F000, size: 4096 }).unwrap();
    w.page_tables(PageTables { pml4_phys: 0x7D46_C000, direct_map_base: DIRECT_MAP_BASE, direct_map_size: 0x8000_0000, kernel_virt_base: KERNEL_BASE, vmap_base: VMAP_BASE }).unwrap();
    w.loader_log(b"bitsboot: BitsOS V2 loader (G0)\n").unwrap();
    w.memory_map(qemu_map().into_iter()).unwrap();
    w.len()
}

fn new_boot<'a>(map: &'a [MemDesc], initrd: Option<(u64, u64)>, hz: u64, old: &BootInfo<'_>) -> NewBoot<'a> {
    NewBoot {
        image: KernelImage { phys_base: 0x4000_0000, phys_size: 0x6000, virt_base: KERNEL_BASE, entry: KERNEL_BASE + 0x10 },
        initrd,
        tables: PageTables { pml4_phys: 0x5000_0000, direct_map_base: DIRECT_MAP_BASE, direct_map_size: 0x8000_0000, kernel_virt_base: KERNEL_BASE, vmap_base: VMAP_BASE },
        tsc: handover::next_tsc(old, hz, tsc_source::PIT, false),
        map,
    }
}

#[test]
fn bootinfo_hands_on_everything_but_the_new_kernel() {
    let mut buf = vec![0u8; 64 * 1024];
    old_bootinfo(&mut buf, 0, true);
    let old = BootInfo::parse(&buf).unwrap();
    let map = qemu_map();
    let bytes = handover::build(&old, &new_boot(&map, Some((0x4030_0000, 12345)), 3_878_431_300, &old), handover::capacity(&old)).unwrap();
    assert_eq!(&bytes[20..24], &(64u32 * 1024).to_le_bytes(), "capacity of the KEEP block kept");
    let nb = BootInfo::parse(&bytes).unwrap();
    assert_eq!(nb.reload().unwrap().counter, 1);
    assert_eq!(nb.config(), old.config());
    assert_eq!(nb.find(99), Some(&b"from a newer loader"[..]), "unknown tags handed on");
    assert_eq!(nb.edid(), old.edid(), "EDID handed on");
    assert!(nb.edid().is_some());
    assert_eq!(nb.crash_page(), old.crash_page());
    assert_eq!(nb.trampoline(), old.trampoline());
    assert_eq!(nb.reload_stub(), old.reload_stub());
    assert_eq!(nb.generation(), old.generation());
    assert_eq!(nb.handover(), old.handover());
    assert_eq!(nb.loader_log(), old.loader_log());
    let t = nb.tsc().unwrap();
    assert_eq!((t.hint_hz, t.calibrated_hz, t.source, t.calibrated_at_reload), (3_900_000_000, 3_878_431_300, tsc_source::PIT, 0));
    let mods: Vec<_> = nb.modules().collect();
    assert_eq!(mods.len(), 2);
    assert_eq!((mods[0].kind, mods[0].phys, mods[0].size, mods[0].name), (module_kind::INITRAMFS, 0x4030_0000, 12345, &b"INITRD"[..]));
    assert_eq!(mods[1].name, b"rtw.bin", "firmware modules handed on");
    assert_eq!(nb.kernel_image().unwrap().entry, KERNEL_BASE + 0x10);
    assert_eq!(nb.page_tables().unwrap().pml4_phys, 0x5000_0000);
    assert_eq!(nb.memory_map().unwrap().len(), map.len());
    let tags: Vec<u32> = nb.records().map(|r| r.tag).collect();
    assert_eq!(*tags.last().unwrap(), tag::MEMORY_MAP, "memory map last");
    assert_eq!(tags.iter().filter(|&&t| t == tag::MEMORY_MAP).count(), 1);

    // Without a new initramfs the running one is handed on.
    let kept = handover::build(&old, &new_boot(&map, None, 1, &old), handover::capacity(&old)).unwrap();
    let kb = BootInfo::parse(&kept).unwrap();
    assert_eq!(kb.modules().next().unwrap().phys, 0x7E03_F000);
    assert_eq!(handover::old_initrds(&old), vec![(0x7E03_F000, 0x7E03_F000 + 0x8E123)]);
}

#[test]
fn ten_reloads_count_up_and_keep_the_size() {
    let mut buf = vec![0u8; 32 * 1024];
    old_bootinfo(&mut buf, 0, false);
    let map = qemu_map();
    let mut cur = buf.clone();
    let mut sizes = Vec::new();
    for n in 1..=10u32 {
        let old = BootInfo::parse(&cur).unwrap();
        let next = handover::build(&old, &new_boot(&map, Some((0x4030_0000, 9000 + n as u64)), 3_878_000_000 + n as u64, &old), handover::capacity(&old)).unwrap();
        let nb = BootInfo::parse(&next).unwrap();
        assert_eq!(nb.reload().unwrap().counter, n);
        let t = nb.tsc().unwrap();
        assert_eq!((t.calibrated_hz, t.calibrated_at_reload), (3_878_000_000 + n as u64, 0), "measured at the cold boot, handed on");
        // A kernel that corrected the handed-on rate against the HPET (INT-A1e) records it as its own.
        let mine = handover::next_tsc(&nb, 3_792_876_000, tsc_source::HPET, true);
        assert_eq!((mine.hint_hz, mine.calibrated_hz, mine.source, mine.calibrated_at_reload), (3_900_000_000, 3_792_876_000, tsc_source::HPET, n));
        assert_eq!(handover::next_tsc(&nb, 1, tsc_source::HPET, false).source, t.source, "kept: the origin stays");
        sizes.push(next.len());
        cur = buf.clone();
        cur[..next.len()].copy_from_slice(&next);
    }
    assert!(sizes.windows(2).all(|w| w[0] == w[1]), "{sizes:?}");
    // A BootInfo that would not fit its KEEP block is an error, not a cut.
    let small = vec![0u8; 512];
    let mut tiny = small.clone();
    let mut w = Writer::new(&mut tiny).unwrap();
    w.config(&[b'x'; 200]).unwrap();
    let tb = BootInfo::parse(&tiny).unwrap();
    assert!(matches!(handover::build(&tb, &new_boot(&map, None, 1, &tb), handover::capacity(&tb)), Err(handover::BuildError::Write(_))));
    // A capacity field no allocation can serve is refused, not a panic.
    assert!(matches!(handover::build(&tb, &new_boot(&map, None, 1, &tb), usize::MAX / 2), Err(handover::BuildError::NoMemory { .. })));
}

#[test]
fn fnv1a_64_known_values() {
    let h = |s: &[u8]| {
        let mut f = Fnv::new();
        f.update(s);
        f.finish()
    };
    assert_eq!(h(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(h(b"a"), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(h(b"foobar"), 0x8594_4171_f739_67e8);
}

#[test]
fn order_for_blocks() {
    use crate::stage::order_for;
    assert_eq!(order_for(0), 0);
    assert_eq!(order_for(4096), 0);
    assert_eq!(order_for(4097), 1);
    assert_eq!(order_for(0x6000), 3);
    assert_eq!(order_for(1 << 20), 8);
}

#[test]
fn a_cpu_parked_by_nmi_refuses_the_reload() {
    use hal::kexec_stub::StopReport;
    let ok = StopReport { others: 15, by_irq: 15, ipis: 17, ..StopReport::default() };
    assert!(crate::tool::stop_verdict(&ok).is_ok(), "all parked by interrupt (re-sends allowed): go on");
    assert!(crate::tool::stop_verdict(&StopReport::default()).is_ok(), "one CPU: go on");
    let stuck = StopReport { others: 15, by_irq: 14, by_nmi: 1, first_nmi_cpu: Some(7), ..StopReport::default() };
    let e = crate::tool::stop_verdict(&stuck).expect_err("an NMI-parked CPU may hold any lock: refuse");
    assert!(e.contains("CPU 7"), "{e}");
}

#[test]
fn a_refused_busy_take_leaves_the_flag_set() {
    // The only host test that touches the flag (no other host test takes it).
    use crate::tool::Busy;
    let first = Busy::take().expect("free at start");
    for _ in 0..3 {
        assert!(Busy::take().is_none(), "held: every further take is refused");
    }
    assert!(Busy::take().is_none(), "a refused take did not clear the flag");
    drop(first);
    let again = Busy::take().expect("free again once the holder drops it");
    drop(again);
}

// RB-10: the BootInfo refreshed at the jump.
#[test]
fn refresh_swaps_config_and_generation_and_keeps_the_rest() {
    let mut buf = vec![0u8; 32 * 1024];
    old_bootinfo(&mut buf, 0, true);
    let old = BootInfo::parse(&buf).unwrap();
    let map = qemu_map();
    let cap = handover::capacity(&old);
    let staged = handover::build(&old, &new_boot(&map, None, 1, &old), cap).unwrap();
    let fresh = b"token=x\nmcp_port=8765\nsnd_output=hdmi\n";
    let g = Generation { number: 3, reason: gen_reason::NEWEST_OK, skipped: 0, flags: 0 };
    let out = handover::refresh(&staged, cap, Some(fresh), Some(g)).unwrap();
    let (sb, nb) = (BootInfo::parse(&staged).unwrap(), BootInfo::parse(&out).unwrap());
    assert_eq!(&out[20..24], &staged[20..24], "capacity kept");
    assert_eq!(nb.config(), Some(&fresh[..]));
    assert_eq!(nb.generation(), Some(g));
    let tags = |b: &BootInfo<'_>| b.records().map(|r| r.tag).collect::<Vec<u32>>();
    assert_eq!(tags(&nb), tags(&sb), "same records in the same order");
    for (a, b) in sb.records().zip(nb.records()) {
        if a.tag != tag::CONFIG && a.tag != tag::GENERATION {
            assert_eq!(a.payload, b.payload, "tag {} unchanged", a.tag);
        }
    }
    // Nothing to change: the same bytes.
    assert_eq!(handover::refresh(&staged, cap, None, None).unwrap(), staged);
    // Only the generation: the CONFIG stays the boot copy.
    let only = handover::refresh(&staged, cap, None, Some(g)).unwrap();
    assert_eq!(BootInfo::parse(&only).unwrap().config(), old.config());
    // A text larger than the KEEP block: refused, so the caller keeps the staged bytes.
    let big = vec![b'#'; cap];
    assert!(matches!(handover::refresh(&staged, cap, Some(&big), None), Err(handover::BuildError::Write(_))));
    assert_eq!(handover::refresh(b"not a bootinfo", cap, None, None), Err(handover::BuildError::Unreadable));
    // A staged BootInfo without CONFIG gets one before the memory map.
    let mut b2 = vec![0u8; 8 * 1024];
    let mut w = Writer::new(&mut b2).unwrap();
    w.reload(Reload { counter: 1 }).unwrap();
    w.memory_map(qemu_map().into_iter()).unwrap();
    let n = w.len();
    let o2 = handover::refresh(&b2[..n], 8 * 1024, Some(b"a=1\n"), None).unwrap();
    let p2 = BootInfo::parse(&o2).unwrap();
    assert_eq!(p2.config(), Some(&b"a=1\n"[..]));
    assert_eq!(p2.records().map(|r| r.tag).collect::<Vec<_>>(), vec![tag::RELOAD, tag::CONFIG, tag::MEMORY_MAP]);
}

#[test]
fn reload_config_takes_the_stick_file_with_its_guards() {
    use crate::refresh::{FileNow, Outcome, choose_config, effective};
    let boot: &[u8] = b"token=t\nmcp_port=8765\nnvme_write=off\n";
    let t = |s: &[u8]| FileNow::Text(s.to_vec());
    // Unchanged, and a settingsd key written since the boot.
    assert_eq!(choose_config(boot, t(boot)), (None, Outcome::Unchanged));
    let added = b"token=t\nmcp_port=8765\nnvme_write=off\nsnd_output=hdmi\n";
    assert_eq!(choose_config(boot, t(added)), (Some(added.to_vec()), Outcome::Fresh));
    // Missing, unreadable, too large, a stick that does not answer: the boot copy.
    assert_eq!(choose_config(boot, FileNow::Missing), (None, Outcome::Missing));
    assert_eq!(choose_config(boot, FileNow::Failed(uapi::errno::EIO)), (None, Outcome::Failed(uapi::errno::EIO)));
    assert_eq!(choose_config(boot, FileNow::Failed(uapi::errno::EFBIG)).0, None);
    assert_eq!(choose_config(boot, FileNow::Slow), (None, Outcome::Slow));
    assert!(Outcome::Slow.fell_back() && !Outcome::Fresh.fell_back());
    // A guarded key changed by hand since the boot (any spelling): the other keys come, it stays as at boot.
    for f in [&b"token=t\nmcp_port=8765\nnvme_write=on\nsnd_output=hdmi\n"[..], b"token=t\nmcp_port=8765\nNVME_WRITE = on\nsnd_output=hdmi", b"\xef\xbb\xbfnvme_write=on\ntoken=t\nsnd_output=hdmi\n"] {
        let (c, o) = choose_config(boot, t(f));
        let c = c.unwrap();
        assert_eq!(o, Outcome::KeptKeys(vec!["nvme_write"]), "{}", String::from_utf8_lossy(f));
        assert_eq!(effective(&c, "nvme_write").as_deref(), Some("off"));
        assert_eq!(effective(&c, "snd_output").as_deref(), Some("hdmi"));
        assert_eq!(effective(&c, "token").as_deref(), Some("t"));
        assert!(fs_fat::protect::config_guard(boot, &c).is_ok());
        assert!(!String::from_utf8_lossy(&c).to_ascii_lowercase().contains("nvme_write=on") && !String::from_utf8_lossy(&c).to_ascii_lowercase().contains("nvme_write = on"));
    }
    // A guarded key added where the boot had none: dropped.
    let (c, o) = choose_config(b"a=1\n", t(b"a=2\nstick_write=on\n"));
    assert_eq!((c.as_deref(), o), (Some(&b"a=2\n# reload: these keys stay as at boot until the next firmware start\n"[..]), Outcome::KeptKeys(vec!["stick_write"])));
    // The profile is bitsboot's choice at the firmware start.
    let (c, o) = choose_config(b"a=1\n", t(b"a=1\nprofile=gaming-pc\nb=2"));
    assert_eq!(o, Outcome::KeptKeys(vec!["profile"]));
    let c = c.unwrap();
    assert_eq!((effective(&c, "profile").as_deref(), effective(&c, "b").as_deref()), (None, Some("2")));
    // Safe mode: bitsboot's wrapped copy stays (nothing writes the stick then).
    let safe = b"panic=halt\na=1\nprofile=gaming-pc\nprofile_reason=dmi\nstick_write=off\n";
    assert_eq!(choose_config(safe, t(b"a=1\nb=2\n")), (None, Outcome::SafeMode));
    // An empty boot copy (no CONFIG record) and a new file.
    assert_eq!(choose_config(b"", t(b"x=1\n")), (Some(b"x=1\n".to_vec()), Outcome::Fresh));
}

#[test]
fn reload_config_reads_every_spelling_of_a_kept_key() {
    use crate::refresh::{FileNow, Outcome, choose_config, effective};
    let t = |s: &[u8]| FileNow::Text(s.to_vec());
    let boot: &[u8] = b"token=t\r\nnvme_write=off\r\nstick_write=off\r\n";
    // A CRLF file with a settingsd key: taken as it is.
    let crlf = b"token=t\r\nnvme_write=off\r\nstick_write=off\r\nsnd_output=hdmi\r\n";
    assert_eq!(choose_config(boot, t(crlf)), (Some(crlf.to_vec()), Outcome::Fresh));
    // Upper case, a BOM, CRLF: each counted as the key it spells, and the boot value stays.
    let f = b"NVME_WRITE=on\r\n\xef\xbb\xbfstick_write=on\r\ntoken=t\r\nsnd_output=hdmi\r\n";
    let (c, o) = choose_config(boot, t(f));
    let c = c.unwrap();
    assert_eq!(o, Outcome::KeptKeys(vec!["nvme_write", "stick_write"]));
    for k in ["nvme_write", "stick_write"] {
        assert_eq!(effective(&c, k).as_deref(), Some("off"), "{k}");
    }
    assert_eq!(effective(&c, "snd_output").as_deref(), Some("hdmi"));
    assert!(fs_fat::protect::config_guard(boot, &c).is_ok());
    // The profile in another case is the profile too (the kept list is complete, the line goes).
    let (c, o) = choose_config(b"a=1\n", t(b"a=1\nPROFILE=gaming-pc\nb=2\n"));
    assert_eq!(o, Outcome::KeptKeys(vec!["profile"]));
    assert!(!String::from_utf8_lossy(&c.unwrap()).contains("PROFILE"));
    // A commented-out guarded line is no key.
    let commented = b"token=t\r\nnvme_write=off\r\nstick_write=off\r\n# nvme_write=on\r\n";
    assert_eq!(choose_config(boot, t(commented)).1, Outcome::Fresh);
    // An earlier read still running (dry run): the boot copy, and a name of its own.
    assert_eq!(choose_config(boot, FileNow::Busy), (None, Outcome::Busy));
    assert!(Outcome::Busy.fell_back() && Outcome::Busy.name() == "boot_copy_busy");
}

#[test]
fn reload_rebuild_keeps_the_generation_when_the_config_does_not_fit() {
    use crate::refresh::{Outcome, Plan, Rebuilt, describe, rebuild};
    let mut buf = vec![0u8; 32 * 1024];
    old_bootinfo(&mut buf, 0, true);
    let old = BootInfo::parse(&buf).unwrap();
    let map = qemu_map();
    let cap = handover::capacity(&old);
    let staged = handover::build(&old, &new_boot(&map, None, 1, &old), cap).unwrap();
    let g = Generation { number: 5, reason: gen_reason::NEWEST_OK, skipped: 0, flags: 0 };
    let plan = |config: Option<Vec<u8>>, generation| Plan { config, outcome: Outcome::Fresh, generation, switched: false, boot_bytes: 163 };
    // Fits: both in.
    let p = plan(Some(b"a=1\n".to_vec()), Some(g));
    let r = rebuild(&staged, cap, &p);
    assert!(r.config && r.generation && r.error.is_none());
    assert!(describe(&p, &r, cap).contains("the stick's CONFIG.TXT (4 bytes, boot copy 163); generation 5 handed on as confirmed"));
    // Too large with a generation: the generation alone.
    let p = plan(Some(vec![b'#'; cap]), Some(g));
    let r = rebuild(&staged, cap, &p);
    assert!(!r.config && r.generation && matches!(r.error, Some(handover::BuildError::Write(_))));
    let nb = BootInfo::parse(r.bytes.as_ref().unwrap()).unwrap();
    assert_eq!((nb.config(), nb.generation()), (BootInfo::parse(&staged).unwrap().config(), Some(g)));
    let d = describe(&p, &r, cap);
    assert!(d.starts_with("CONFIG: the boot copy (163 bytes): the stick's CONFIG.TXT") && d.contains("does not fit") && d.contains("handed on as confirmed"), "{d}");
    // Too large alone: nothing to hand on.
    let r = rebuild(&staged, cap, &plan(Some(vec![b'#'; cap]), None));
    assert!(r.bytes.is_none() && !r.config && !r.generation && r.error.is_some());
    // Nothing planned: nothing built.
    assert_eq!(rebuild(&staged, cap, &plan(None, None)), Rebuilt { bytes: None, config: false, generation: false, error: None });
}

#[test]
fn reload_hands_a_confirmed_try_on_as_ok() {
    use crate::refresh::refreshed_generation;
    let g = |reason| Some(Generation { number: 5, reason, skipped: 0, flags: 0 });
    assert_eq!(refreshed_generation(g(gen_reason::NEWEST_TRY), Some(true)), g(gen_reason::NEWEST_OK));
    assert_eq!(refreshed_generation(g(gen_reason::NEWEST_TRY), Some(false)), None, "still on trial");
    assert_eq!(refreshed_generation(g(gen_reason::NEWEST_TRY), None), None, "stick not listed");
    assert_eq!(refreshed_generation(g(gen_reason::FALLBACK_AFTER_TRY), Some(true)), None);
    assert_eq!(refreshed_generation(g(gen_reason::NEWEST_OK), Some(true)), None);
    assert_eq!(refreshed_generation(None, Some(true)), None);
}

#[test]
fn reload_into_another_generation_hands_its_record_on() {
    use crate::refresh::{Outcome, Plan, describe, rebuild};
    let mut buf = vec![0u8; 32 * 1024];
    old_bootinfo(&mut buf, 0, true);
    let old = BootInfo::parse(&buf).unwrap();
    let map = qemu_map();
    let cap = handover::capacity(&old);
    let staged = handover::build(&old, &new_boot(&map, None, 1, &old), cap).unwrap();
    // RB-9 system_pin 2 now: the new kernel is generation 2, PINNED (the running one was 3).
    let g = Generation { number: 2, reason: gen_reason::PINNED, skipped: 3, flags: 0 };
    let p = Plan { config: None, outcome: Outcome::Unchanged, generation: Some(g), switched: true, boot_bytes: 22 };
    let r = rebuild(&staged, cap, &p);
    assert!(r.generation && r.error.is_none());
    assert_eq!(BootInfo::parse(r.bytes.as_ref().unwrap()).unwrap().generation(), Some(g));
    let d = describe(&p, &r, cap);
    assert!(d.contains("the new kernel is told it is generation 2 (PINNED)"), "{d}");
    // A BootInfo without a GENERATION record (not started by bitsboot) gets one.
    let mut small = vec![0u8; 4096];
    let mut w = Writer::new(&mut small).unwrap();
    w.config(b"a=1\n").unwrap();
    let len = w.len();
    let out = handover::refresh(&small[..len], 4096, None, Some(g)).unwrap();
    let nb = BootInfo::parse(&out).unwrap();
    assert_eq!((nb.generation(), nb.config()), (Some(g), Some(&b"a=1\n"[..])));
    assert_eq!(BootInfo::parse(&handover::refresh(&small[..len], 4096, None, None).unwrap()).unwrap().generation(), None);
}
