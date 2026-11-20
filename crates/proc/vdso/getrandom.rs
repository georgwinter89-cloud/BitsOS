// L7-SYNC: `__vdso_getrandom` / `getrandom` (Linux 7.3.1's lib/vdso/getrandom.c `__cvdso_getrandom_data`,
// include/vdso/getrandom.h `struct vgetrandom_state`; own code). Random bytes in user space from a per-thread
// state the C library allocates: a ChaCha20 key fetched from the getrandom syscall, re-fetched whenever the
// kernel's generation in the vvar page changed (a reseed; V2 also counts a minute as one), and a batch of
// output kept for small requests. Every 64-byte block made also replaces the key (fast key erasure), and bytes
// handed out are zeroed in the batch, so a later look at the state cannot recover earlier output.
//
// The state must live in memory the kernel wipes on fork (MAP_DROPPABLE): a child then finds generation 0 and
// fetches its own key. The params query (`opaque_len == ~0`) names that mapping as Linux does, and it answers
// -ENOSYS while this kernel's mmap does not take MAP_DROPPABLE (the vvar page says so): glibc then makes the
// syscall every time, which is cheaper than its retrying mmap would be. Anything this function does not
// handle (unknown flags, a state of the wrong size, the RNG not ready, a state in use by an interrupted call)
// is the syscall, so its answers are the kernel's.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{Ordering, compiler_fence};

const SYS_GETRANDOM: u64 = 318;
const GRND_NONBLOCK: u32 = 1;
const GRND_RANDOM: u32 = 2;
const GRND_INSECURE: u32 = 4;
const PROT_READ: u32 = 1;
const PROT_WRITE: u32 = 2;
const MAP_ANONYMOUS: u32 = 0x20;
const MAP_DROPPABLE: u32 = 0x08;
const ENOSYS: isize = 38;
const EFAULT: isize = 14;
/// `MAX_RW_COUNT` (INT_MAX & PAGE_MASK).
const MAX_RW: usize = (i32::MAX as usize) & !0xfff;
const BLOCK: usize = 64;
const BATCH: usize = BLOCK * 3 / 2;
const KEY_WORDS: usize = 8;

/// `struct vgetrandom_state` (144 bytes): one and a half blocks of output, then the key, filled together.
#[repr(C)]
struct State {
    batch_key: [u8; BLOCK * 2],
    generation: u64,
    pos: u8,
    in_use: bool,
}

/// `struct vgetrandom_opaque_params`.
#[repr(C)]
struct OpaqueParams {
    size_of_opaque_state: u32,
    mmap_prot: u32,
    mmap_flags: u32,
    reserved: [u32; 13],
}

fn getrandom_syscall(buf: *mut u8, len: usize, flags: u32) -> isize {
    super::syscall3(SYS_GETRANDOM, buf as u64, len as u64, flags as u64) as isize
}

#[inline(always)]
fn qr(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

/// `__arch_chacha20_blocks_nostack`: `n` blocks of ChaCha20 (nonce 0, 64-bit block counter) into `dst`. The key
/// is copied first, so `dst` may overlap it; the working copies are zeroed before returning.
///
/// # Safety
/// SAFETY (the caller's): `dst` writable for `n * 64` bytes, `key` readable for 32.
unsafe fn chacha_blocks(dst: *mut u8, key: *const u8, counter: &mut u64, n: usize) {
    let mut k = [0u32; KEY_WORDS];
    for (i, w) in k.iter_mut().enumerate() {
        // SAFETY: inside the caller's 32 key bytes.
        *w = u32::from_le_bytes(unsafe { [*key.add(i * 4), *key.add(i * 4 + 1), *key.add(i * 4 + 2), *key.add(i * 4 + 3)] });
    }
    let mut x = [0u32; 16];
    for b in 0..n {
        let mut init = [0u32; 16];
        init[..4].copy_from_slice(&[0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574]);
        init[4..12].copy_from_slice(&k);
        init[12] = *counter as u32;
        init[13] = (*counter >> 32) as u32;
        x = init;
        for _ in 0..10 {
            qr(&mut x, 0, 4, 8, 12);
            qr(&mut x, 1, 5, 9, 13);
            qr(&mut x, 2, 6, 10, 14);
            qr(&mut x, 3, 7, 11, 15);
            qr(&mut x, 0, 5, 10, 15);
            qr(&mut x, 1, 6, 11, 12);
            qr(&mut x, 2, 7, 8, 13);
            qr(&mut x, 3, 4, 9, 14);
        }
        for i in 0..16 {
            let v = x[i].wrapping_add(init[i]).to_le_bytes();
            for (j, byte) in v.iter().enumerate() {
                // SAFETY: block `b` of the caller's `n * 64` bytes.
                unsafe { write_volatile(dst.add(b * BLOCK + i * 4 + j), *byte) };
            }
        }
        *counter = counter.wrapping_add(1);
        wipe(&mut init);
    }
    wipe(&mut x);
    wipe(&mut k);
}

fn wipe<const N: usize>(a: &mut [u32; N]) {
    for w in a.iter_mut() {
        // SAFETY: a plain local.
        unsafe { write_volatile(w, 0) };
    }
}

/// Copies `n` bytes from `src` to `dst` and zeroes them in `src` (forward secrecy of handed-out bytes).
///
/// # Safety
/// SAFETY (the caller's): both ranges valid for `n` bytes and disjoint.
unsafe fn take(dst: *mut u8, src: *mut u8, n: usize) {
    for i in 0..n {
        // SAFETY: the caller's ranges.
        unsafe {
            *dst.add(i) = *src.add(i);
            write_volatile(src.add(i), 0);
        }
    }
}

fn rng_generation() -> u64 {
    super::r64(super::RNG_GEN)
}

/// # Safety
/// As Linux's: `buffer` writable for `len` bytes, `opaque_state` a state of `opaque_len` bytes from the params'
/// mapping (or, for the params query, a `struct vgetrandom_opaque_params`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __vdso_getrandom(buffer: *mut u8, len: usize, flags: u32, opaque_state: *mut u8, opaque_len: usize) -> isize {
    if opaque_len == usize::MAX && buffer.is_null() && len == 0 && flags == 0 {
        if super::r32(super::RNG_FLAGS) & super::RNG_F_DROPPABLE == 0 {
            return -ENOSYS;
        }
        let p = opaque_state as *mut OpaqueParams;
        // SAFETY: the caller's params struct.
        unsafe {
            p.write(OpaqueParams { size_of_opaque_state: core::mem::size_of::<State>() as u32, mmap_prot: PROT_READ | PROT_WRITE, mmap_flags: MAP_DROPPABLE | MAP_ANONYMOUS, reserved: [0; 13] })
        };
        return 0;
    }
    // The state must not straddle a page: pages of a droppable mapping can be zeroed one at a time.
    if (opaque_state as usize & 0xfff) + core::mem::size_of::<State>() > 0x1000 {
        return -EFAULT;
    }
    let ret = len.min(MAX_RW);
    if flags & !(GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE) != 0 || opaque_len != core::mem::size_of::<State>() || super::r32(super::RNG_READY) == 0 {
        return getrandom_syscall(buffer, len, flags);
    }
    if len == 0 {
        return 0;
    }
    let st = opaque_state as *mut State;
    // SAFETY: the caller's state (checked size and page above); all accesses go through raw pointers because a
    // signal handler may run this same function on the same state between any two of them.
    unsafe {
        if read_volatile(&(*st).in_use) {
            return getrandom_syscall(buffer, len, flags);
        }
        write_volatile(&mut (*st).in_use, true);
        let mut retried = false;
        let mut counter = 0u64;
        'generation: loop {
            let current = rng_generation();
            if read_volatile(&(*st).generation) != current {
                // Before the key: a fork right after this line makes the child see 0 and fetch its own.
                write_volatile(&mut (*st).generation, current);
                compiler_fence(Ordering::SeqCst);
                let key = (*st).batch_key.as_mut_ptr().add(BATCH);
                if getrandom_syscall(key, KEY_WORDS * 4, 0) != (KEY_WORDS * 4) as isize {
                    write_volatile(&mut (*st).generation, 0);
                    write_volatile(&mut (*st).in_use, false);
                    return getrandom_syscall(buffer, len, flags);
                }
                write_volatile(&mut (*st).pos, BATCH as u8);
            }
            let mut out = buffer;
            let mut left = ret;
            loop {
                let pos = read_volatile(&(*st).pos) as usize;
                let n = (BATCH - pos.min(BATCH)).min(left);
                if n > 0 {
                    take(out, (*st).batch_key.as_mut_ptr().add(pos), n);
                    write_volatile(&mut (*st).pos, (pos + n) as u8);
                    out = out.add(n);
                    left -= n;
                }
                if left == 0 {
                    compiler_fence(Ordering::SeqCst);
                    // A fork, a dropped page (generation 0) or a reseed meanwhile: once more with a fresh key.
                    if read_volatile(&(*st).generation) != rng_generation() {
                        if retried {
                            write_volatile(&mut (*st).in_use, false);
                            return getrandom_syscall(buffer, len, flags);
                        }
                        retried = true;
                        continue 'generation;
                    }
                    write_volatile(&mut (*st).in_use, false);
                    return ret as isize;
                }
                let blocks = left / BLOCK;
                let key = (*st).batch_key.as_ptr().add(BATCH);
                if blocks > 0 {
                    chacha_blocks(out, key, &mut counter, blocks);
                    out = out.add(blocks * BLOCK);
                    left -= blocks * BLOCK;
                }
                // The batch and the next key in one go: the old key is gone after this.
                chacha_blocks((*st).batch_key.as_mut_ptr(), key, &mut counter, 2);
                write_volatile(&mut (*st).pos, 0);
            }
        }
    }
}
