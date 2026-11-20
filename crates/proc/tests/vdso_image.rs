//! L7-SYNC: the vDSO image run on the build host (x86-64 Linux) with a hand-filled vvar page: getrandom's params
//! query, its ChaCha20 output against `vfs::random::block` (RFC 8439, checked in vfs's own tests), the key
//! refresh on a new generation, every fallback to the syscall, and both try-unlock helpers.

use core::arch::asm;
use proc::vdso::layout::*;


fn image_pages() -> u64 {
    proc::vdso::image_pages()
}

const PAGE: usize = 4096;

fn sys(nr: u64, a: [u64; 6]) -> i64 {
    let r: i64;
    // SAFETY: plain Linux system calls of the test process on its own memory.
    unsafe {
        asm!("syscall", inlateout("rax") nr as i64 => r, in("rdi") a[0], in("rsi") a[1], in("rdx") a[2], in("r10") a[3],
            in("r8") a[4], in("r9") a[5], lateout("rcx") _, lateout("r11") _, options(nostack))
    };
    r
}

struct Mapped {
    base: *mut u8,
}

impl Mapped {
    /// The vvar page and the image after it, as the kernel maps them (the text read+exec).
    fn new() -> Mapped {
        let len = PAGE * (1 + image_pages() as usize);
        let m = sys(9, [0, len as u64, 3, 0x22, u64::MAX, 0]);
        assert!(m > 0, "mmap {m}");
        let base = m as *mut u8;
        // SAFETY: the fresh mapping holds the vvar page and the image pages.
        unsafe { core::ptr::copy_nonoverlapping(proc::vdso::image().as_ptr(), base.add(PAGE), proc::vdso::image().len()) };
        assert_eq!(sys(10, [m as u64 + PAGE as u64, (len - PAGE) as u64, 5, 0, 0, 0]), 0, "mprotect");
        Mapped { base }
    }
    fn w32(&self, off: usize, v: u32) {
        // SAFETY: inside the writable vvar page.
        unsafe { (self.base.add(off) as *mut u32).write_volatile(v) }
    }
    fn w64(&self, off: usize, v: u64) {
        // SAFETY: as `w32`.
        unsafe { (self.base.add(off) as *mut u64).write_volatile(v) }
    }
    /// The address of the exported `name` (DT_HASH's chain count bounds the symbol table).
    fn sym(&self, name: &str) -> usize {
        let img = proc::vdso::image();
        let rd = |o: usize, n: usize| -> u64 { img[o..o + n].iter().rev().fold(0u64, |a, &b| a << 8 | b as u64) };
        let (phoff, phnum) = (rd(32, 8) as usize, rd(56, 2) as usize);
        let dynoff = (0..phnum).map(|i| phoff + i * 56).find(|&p| rd(p, 4) == 2).map(|p| rd(p + 8, 8) as usize).expect("PT_DYNAMIC");
        let (mut symtab, mut strtab, mut hash) = (0, 0, 0);
        for i in 0.. {
            let (tag, val) = (rd(dynoff + i * 16, 8), rd(dynoff + i * 16 + 8, 8) as usize);
            match tag {
                0 => break,
                4 => hash = val,
                5 => strtab = val,
                6 => symtab = val,
                _ => {}
            }
        }
        let n = rd(hash + 4, 4) as usize;
        for i in 0..n {
            let e = symtab + i * 24;
            let s = strtab + rd(e, 4) as usize;
            let end = img[s..].iter().position(|&c| c == 0).unwrap_or(0);
            if &img[s..s + end] == name.as_bytes() {
                return self.base as usize + PAGE + rd(e + 8, 8) as usize;
            }
        }
        panic!("no {name} in the image");
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        sys(11, [self.base as u64, (PAGE * (1 + image_pages() as usize)) as u64, 0, 0, 0, 0]);
    }
}

type GetrandomFn = unsafe extern "C" fn(*mut u8, usize, u32, *mut u8, usize) -> isize;
type TryUnlock64 = unsafe extern "C" fn(*mut u32, u32, *mut u64) -> u32;
type TryUnlock32 = unsafe extern "C" fn(*mut u32, u32, *mut u32) -> u32;

/// A 144-byte `vgetrandom_state` in a page of its own (8-aligned, page offset `at`).
#[repr(C, align(4096))]
struct StatePage([u8; PAGE]);

const GEN: usize = 128;
const POS: usize = 136;
const IN_USE: usize = 137;
const STATE: usize = 144;

#[test]
fn getrandom_params_need_droppable_mmap() {
    let m = Mapped::new();
    // SAFETY: the image's own function, called as glibc does.
    let gr: GetrandomFn = unsafe { core::mem::transmute(m.sym("__vdso_getrandom")) };
    let mut p = [0u32; 16];
    // SAFETY: a params struct of 64 bytes.
    let r = unsafe { gr(core::ptr::null_mut(), 0, 0, p.as_mut_ptr() as *mut u8, usize::MAX) };
    assert_eq!(r, -38, "no MAP_DROPPABLE: the query says ENOSYS, glibc then makes the syscall");
    m.w32(RNG_FLAGS, RNG_F_DROPPABLE);
    // SAFETY: as above.
    let r = unsafe { gr(core::ptr::null_mut(), 0, 0, p.as_mut_ptr() as *mut u8, usize::MAX) };
    assert_eq!((r, p[0], p[1], p[2]), (0, STATE as u32, 3, 0x28), "size 144, PROT_READ|PROT_WRITE, MAP_DROPPABLE|MAP_ANONYMOUS");
    assert!(p[3..].iter().all(|&w| w == 0), "reserved words zeroed");
}

#[test]
fn getrandom_output_is_chacha20_of_the_state_key() {
    let m = Mapped::new();
    // SAFETY: as in the test above.
    let gr: GetrandomFn = unsafe { core::mem::transmute(m.sym("getrandom")) };
    let mut page = Box::new(StatePage([0; PAGE]));
    let st = page.0.as_mut_ptr();
    let call = |buf: &mut [u8], flags: u32, len: usize| -> isize {
        // SAFETY: `buf` and the state are ours.
        unsafe { gr(buf.as_mut_ptr(), buf.len(), flags, st, len) }
    };
    let mut out = [0u8; 64];
    // RNG not ready: the host's syscall fills the buffer, the state stays untouched.
    assert_eq!(call(&mut out, 0, STATE), 64);
    assert!(page.0[..STATE].iter().all(|&b| b == 0), "state untouched while the RNG is not ready");
    m.w32(RNG_READY, 1);
    m.w64(RNG_GEN, 7);
    // A known key, the current generation and an empty batch: 64 bytes are block 0, the batch blocks 1 and 2.
    let key: [u8; 32] = core::array::from_fn(|i| (i * 7 + 1) as u8);
    page.0[96..128].copy_from_slice(&key);
    page.0[GEN..GEN + 8].copy_from_slice(&7u64.to_le_bytes());
    page.0[POS] = 96;
    let k = vfs::random::key_from_bytes(&key);
    let block = |c: u32| vfs::random::block(&k, c, &[0; 3]);
    assert_eq!(call(&mut out, 0, STATE), 64);
    assert_eq!(out, block(0), "a whole block goes straight to the caller");
    let (b1, b2) = (block(1), block(2));
    assert_eq!(&page.0[..64], &b1[..], "the batch is block 1");
    assert_eq!(&page.0[64..128], &b2[..], "the rest of the batch and the next key are block 2");
    assert_eq!((page.0[POS], page.0[IN_USE]), (0, 0));
    let mut small = [0u8; 32];
    assert_eq!(call(&mut small, 0, STATE), 32);
    assert_eq!(&small[..], &b1[..32], "small requests come from the batch");
    assert!(page.0[..32].iter().all(|&b| b == 0), "handed-out bytes are wiped");
    assert_eq!(page.0[POS], 32);
    // A new generation: a fresh key from the syscall, the old batch dropped.
    m.w64(RNG_GEN, 8);
    let old_key: Vec<u8> = page.0[96..128].to_vec();
    assert_eq!(call(&mut small, 0, STATE), 32);
    assert_eq!(u64::from_le_bytes(page.0[GEN..GEN + 8].try_into().unwrap()), 8);
    assert_ne!(&page.0[96..128], &old_key[..], "new key");
    assert_ne!(&small[..], &b1[32..64], "not the old batch");
    // Large and odd lengths.
    let mut big = vec![0u8; 1000];
    assert_eq!(call(&mut big, 0, STATE), 1000);
    assert!(big.iter().any(|&b| b != 0));
    assert_eq!(call(&mut out[..0], 0, STATE), 0, "length 0");
    // Fallbacks: the syscall answers (the host's), the state is left alone.
    let pos = page.0[POS];
    page.0[IN_USE] = 1;
    assert_eq!(call(&mut small, 0, STATE), 32, "in use (a signal handler): the syscall");
    assert_eq!(page.0[POS], pos);
    page.0[IN_USE] = 0;
    assert_eq!(call(&mut small, 8, STATE), -22, "unknown flag: the syscall's EINVAL");
    assert_eq!(call(&mut small, 0, 100), 32, "wrong state size: the syscall");
    assert_eq!(page.0[POS], pos);
    // SAFETY: a state that would cross the page end.
    let r = unsafe { gr(small.as_mut_ptr(), 32, 0, st.add(PAGE - 100), STATE) };
    assert_eq!(r, -14, "a state across a page edge is EFAULT");
}

#[test]
fn try_unlock_clears_the_pending_pointer_only_on_success() {
    let m = Mapped::new();
    // SAFETY: the image's own functions.
    let (f64, f32): (TryUnlock64, TryUnlock32) =
        unsafe { (core::mem::transmute(m.sym("__vdso_futex_robust_list64_try_unlock")), core::mem::transmute(m.sym("__vdso_futex_robust_list32_try_unlock"))) };
    let (mut lock, mut pop) = (77u32, 5u64);
    // SAFETY: our own words.
    assert_eq!(unsafe { f64(&mut lock, 77, &mut pop) }, 77);
    assert_eq!((lock, pop), (0, 0));
    let (mut lock, mut pop) = (77u32 | 0x8000_0000, 5u64);
    // SAFETY: as above.
    assert_eq!(unsafe { f64(&mut lock, 77, &mut pop) }, 77 | 0x8000_0000, "FUTEX_WAITERS: the word is returned");
    assert_eq!((lock, pop), (77 | 0x8000_0000, 5), "and nothing changes");
    let (mut lock, mut pop) = (9u32, [u32::MAX; 2]);
    // SAFETY: as above.
    assert_eq!(unsafe { f32(&mut lock, 9, pop.as_mut_ptr()) }, 9);
    assert_eq!((lock, pop), (0, [0, u32::MAX]), "a 32-bit pending pointer: 4 bytes");
}
