// The vvar page (AB-14): the data the kernel publishes and the vDSO reads. One file for both sides: the kernel
// has it as `proc::vdso::layout`, the vDSO (`crates/proc/vdso/vdso.rs`, built by `crates/proc/build.rs`)
// `include!`s it. Only constants here, so both compile it alike. The layout is private between this kernel and
// its own vDSO (Linux's `struct vdso_data` is not an ABI either); programs only see the exported functions.
//
// Protocol (Linux `vdso_write_begin`/`vdso_read_begin`, linux-6.6/include/vdso/helpers.h): the writer makes
// `SEQ` odd, stores the fields, makes it even again; a reader takes an even `SEQ`, reads the TSC and the fields
// and retries when `SEQ` changed meanwhile. x86 keeps stores in order and loads in order, so compiler barriers
// suffice on both sides.

/// Size of the vvar mapping: one page, mapped read-only right before the vDSO's ELF header.
pub const VVAR_SIZE: usize = 4096;

/// u32 sequence count, odd while the kernel writes.
pub const SEQ: usize = 0;
/// u32 `MODE_*`: whether the vDSO may answer from the TSC (Linux `vclock_mode`).
pub const MODE: usize = 4;
/// u64 TSC value the monotonic base was taken at.
pub const BASE_TSC: usize = 8;
/// u64 CLOCK_MONOTONIC in ns at `BASE_TSC`.
pub const BASE_NS: usize = 16;
/// u64 ns per TSC tick in 32.32 fixed point (the time crate's `ns_mult`; the shift is always `SHIFT`).
pub const MULT: usize = 24;
/// i64 CLOCK_REALTIME minus CLOCK_MONOTONIC (Linux `offs_real`).
pub const REAL_OFF: usize = 32;
/// i64 CLOCK_TAI minus CLOCK_REALTIME (V2: 0, TAI reads as REALTIME as the syscall does).
pub const TAI_OFF: usize = 40;
/// i64 CLOCK_BOOTTIME minus CLOCK_MONOTONIC (V2: 0, no suspend).
pub const BOOT_OFF: usize = 48;
/// u32 resolution in ns of the high-resolution clocks (`clock_getres`).
pub const RES_NS: usize = 56;
/// u32 resolution in ns of the COARSE clocks.
pub const COARSE_RES_NS: usize = 60;
/// u32 `F_*` bits: which instructions the vDSO may use.
pub const FLAGS: usize = 64;
/// Start of the room kept for time namespace offsets (SX: CLONE_NEWTIME; nothing there yet). The page ends at
/// `VVAR_SIZE`.
pub const TIMENS: usize = 128;

/// The shift that goes with `MULT`.
pub const SHIFT: u32 = 32;

/// No TSC answers: every call makes the real syscall (not published yet, or the TSC is not usable).
pub const MODE_NONE: u32 = 0;
/// Answers from the TSC with `BASE_TSC`/`BASE_NS`/`MULT`.
pub const MODE_TSC: u32 = 1;

/// rdtscp exists (an ordered TSC read; `lfence; rdtsc` without it).
pub const F_RDTSCP: u32 = 1;
/// rdpid exists (getcpu reads IA32_TSC_AUX with it).
pub const F_RDPID: u32 = 2;
/// IA32_TSC_AUX holds `(node << 12) | cpu` on every CPU (fewer than 4096 CPUs): getcpu may answer from it.
pub const F_GETCPU: u32 = 4;
