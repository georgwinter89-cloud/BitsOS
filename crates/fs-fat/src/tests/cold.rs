//! FS-14b: a cold start reads each byte from the stick once (plan/notes/desk-minipc-w11.md "Cold vs warm
//! start": Brave's first start read 4.5 GB for a 575 MB folder). Files mapped by several "processes" in their
//! own fault orders, faulted at the same time by two threads, and read with read() in small and large pieces:
//! the device reads at most 1.2 x the bytes of the files, and a second pass reads nothing.

use super::util::*;
use std::sync::Arc;
use std::vec;
use std::vec::Vec;
use vfs::abi::O_RDONLY;
use vfs::mmap::{self, MapBacking, MAP_PRIVATE, PROT_EXEC};
use vfs::ops::Ctx;

const P: u64 = 4096;
const EXE: usize = 6 * 1024 * 1024 + 1234;
const LIB: usize = 3 * 1024 * 1024 + 77;
const PAK: usize = 4 * 1024 * 1024 + 4000;

fn map(f: &vfs::OpenFile, len: usize) -> Arc<vfs::mmap::FileMap> {
    mmap::check(f, 1 | PROT_EXEC, MAP_PRIVATE, 0, len as u64).unwrap().backing
}

#[test]
fn cold_start_reads_each_byte_once() {
    let dir = scratch("cold");
    let img = mkfs(&dir, 40, 1, "BITSOS");
    mkdir(&img, "/app");
    put_bytes(&img, &dir, "/app/exe", &pattern(EXE, 41));
    put_bytes(&img, &dir, "/app/lib.so", &pattern(LIB, 42));
    put_bytes(&img, &dir, "/app/res.pak", &pattern(PAK, 43));
    let dev = load(&img);
    let vol = mount_plain(&dev);
    let ns = view(&vol);
    let c = Ctx::kernel(&ns);
    let exe = c.open(None, b"/boot/app/exe", O_RDONLY, 0).unwrap();
    let lib = c.open(None, b"/boot/app/lib.so", O_RDONLY, 0).unwrap();
    let pak = c.open(None, b"/boot/app/res.pak", O_RDONLY, 0).unwrap();
    let base = dev.read_bytes();

    // Two processes map the executable and fault it in their own orders, one after the other: A jumps around
    // the whole file (every page once, a stride permutation), B goes forward from the middle.
    let (a, b) = (map(&exe, EXE), map(&exe, EXE));
    let pages = (EXE as u64).div_ceil(P);
    for i in 0..pages {
        a.get_page((i * 7919) % pages, false).unwrap();
        b.get_page((pages / 2 + i) % pages, false).unwrap();
    }
    let after_exe = dev.read_bytes() - base;

    // Two threads fault the same library pages at the same time (zygote and renderer starting together).
    dev.slow_reads(300);
    let lpages = (LIB as u64).div_ceil(P);
    let maps: Vec<_> = (0..2).map(|_| map(&lib, LIB)).collect();
    std::thread::scope(|s| {
        for m in &maps {
            s.spawn(move || {
                for i in 0..lpages {
                    m.get_page((i * 3) % lpages, false).unwrap();
                }
            });
        }
    });
    dev.slow_reads(0);
    let after_lib = dev.read_bytes() - base - after_exe;

    // A resource file read with read(): its start in 4 KiB pieces, a jump back and forth, the rest in 1 MiB.
    let mut small = vec![0u8; 4096];
    for k in 0..64u64 {
        pak.pread(k * P, &mut small).unwrap();
    }
    pak.pread(3 * 1024 * 1024, &mut small).unwrap();
    pak.pread(100 * P, &mut small).unwrap();
    let mut big = vec![0u8; 1 << 20];
    let mut at = 64 * P;
    while (at as usize) < PAK {
        at += pak.pread(at, &mut big).unwrap() as u64;
    }
    let after_pak = dev.read_bytes() - base - after_exe - after_lib;

    let limit = |n: usize| n as u64 * 12 / 10;
    std::eprintln!("cold: exe {after_exe} of {EXE}, lib {after_lib} of {LIB}, pak {after_pak} of {PAK} bytes from the device");
    assert!(after_exe <= limit(EXE), "the mapped executable read {after_exe} bytes for {EXE}");
    assert!(after_lib <= limit(LIB), "the library faulted by two threads read {after_lib} bytes for {LIB}");
    assert!(after_pak <= limit(PAK), "the resource file read {after_pak} bytes for {PAK}");

    // Warm: everything again, nothing from the device; and the bytes are the file's.
    let warm = dev.read_bytes();
    for i in 0..pages {
        a.get_page(i, false).unwrap();
    }
    assert!(ns.read_file("/boot/app/exe", 1 << 24).unwrap() == pattern(EXE, 41), "exe content");
    assert!(ns.read_file("/boot/app/lib.so", 1 << 24).unwrap() == pattern(LIB, 42), "lib content");
    assert!(ns.read_file("/boot/app/res.pak", 1 << 24).unwrap() == pattern(PAK, 43), "pak content");
    assert_eq!(dev.read_bytes(), warm, "a warm pass reads nothing from the device");
}
