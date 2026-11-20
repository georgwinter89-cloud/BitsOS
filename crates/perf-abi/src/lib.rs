//! perf_event_open's ABI after linux-7.3.1/include/uapi/linux/perf_event.h, shared by the kernel's sampling
//! profiler and `bitsprof` (SELFOPT-1): the constants, `Attr` (the attribute as the kernel reads it), the
//! record layouts (`record`), and the ring buffer's space rule (`ring`).
#![no_std]
#![forbid(unsafe_code)]

pub mod attr;
pub mod record;
pub mod ring;

pub use attr::{Attr, AttrError};

pub const PERF_TYPE_HARDWARE: u32 = 0;
pub const PERF_TYPE_SOFTWARE: u32 = 1;
pub const PERF_TYPE_TRACEPOINT: u32 = 2;
pub const PERF_TYPE_HW_CACHE: u32 = 3;
pub const PERF_TYPE_RAW: u32 = 4;
pub const PERF_TYPE_BREAKPOINT: u32 = 5;

pub const SW_CPU_CLOCK: u64 = 0;
pub const SW_TASK_CLOCK: u64 = 1;
pub const SW_DUMMY: u64 = 9;
/// `PERF_COUNT_SW_MAX`: configs at or above it are unknown (ENOENT).
pub const SW_MAX: u64 = 12;

pub const SAMPLE_IP: u64 = 1 << 0;
pub const SAMPLE_TID: u64 = 1 << 1;
pub const SAMPLE_TIME: u64 = 1 << 2;
pub const SAMPLE_ADDR: u64 = 1 << 3;
pub const SAMPLE_READ: u64 = 1 << 4;
pub const SAMPLE_CALLCHAIN: u64 = 1 << 5;
pub const SAMPLE_ID: u64 = 1 << 6;
pub const SAMPLE_CPU: u64 = 1 << 7;
pub const SAMPLE_PERIOD: u64 = 1 << 8;
pub const SAMPLE_STREAM_ID: u64 = 1 << 9;
pub const SAMPLE_BRANCH_STACK: u64 = 1 << 11;
pub const SAMPLE_REGS_USER: u64 = 1 << 12;
pub const SAMPLE_STACK_USER: u64 = 1 << 13;
pub const SAMPLE_WEIGHT: u64 = 1 << 14;
pub const SAMPLE_IDENTIFIER: u64 = 1 << 16;
pub const SAMPLE_REGS_INTR: u64 = 1 << 18;
pub const SAMPLE_PHYS_ADDR: u64 = 1 << 19;
pub const SAMPLE_CGROUP: u64 = 1 << 21;
pub const SAMPLE_WEIGHT_STRUCT: u64 = 1 << 24;
/// `PERF_SAMPLE_MAX` (non-ABI): bits at or above it are EINVAL.
pub const SAMPLE_MAX: u64 = 1 << 25;
/// The sample fields BitsOS fills; asking for another one is EOPNOTSUPP (plan/spec/self-optimise.md, the profiler).
pub const SAMPLE_SUPPORTED: u64 = SAMPLE_IP
    | SAMPLE_TID
    | SAMPLE_TIME
    | SAMPLE_ADDR
    | SAMPLE_READ
    | SAMPLE_CALLCHAIN
    | SAMPLE_ID
    | SAMPLE_CPU
    | SAMPLE_PERIOD
    | SAMPLE_STREAM_ID
    | SAMPLE_IDENTIFIER;

pub const FORMAT_TOTAL_TIME_ENABLED: u64 = 1 << 0;
pub const FORMAT_TOTAL_TIME_RUNNING: u64 = 1 << 1;
pub const FORMAT_ID: u64 = 1 << 2;
pub const FORMAT_GROUP: u64 = 1 << 3;
pub const FORMAT_LOST: u64 = 1 << 4;
pub const FORMAT_MAX: u64 = 1 << 5;

pub const FLAG_FD_NO_GROUP: u64 = 1 << 0;
pub const FLAG_FD_OUTPUT: u64 = 1 << 1;
pub const FLAG_PID_CGROUP: u64 = 1 << 2;
pub const FLAG_FD_CLOEXEC: u64 = 1 << 3;
pub const FLAG_ALL: u64 = FLAG_FD_NO_GROUP | FLAG_FD_OUTPUT | FLAG_PID_CGROUP | FLAG_FD_CLOEXEC;

/// `_IO('$', n)` and friends: type '$' (0x24), the size in bits 16..29, the direction in 30..31.
const fn ioc(dir: u64, nr: u64, size: u64) -> u64 {
    dir << 30 | size << 16 | 0x24 << 8 | nr
}
pub const IOC_ENABLE: u64 = ioc(0, 0, 0);
pub const IOC_DISABLE: u64 = ioc(0, 1, 0);
pub const IOC_REFRESH: u64 = ioc(0, 2, 0);
pub const IOC_RESET: u64 = ioc(0, 3, 0);
pub const IOC_PERIOD: u64 = ioc(1, 4, 8);
pub const IOC_SET_OUTPUT: u64 = ioc(0, 5, 0);
pub const IOC_SET_FILTER: u64 = ioc(1, 6, 8);
pub const IOC_ID: u64 = ioc(2, 7, 8);
pub const IOC_SET_BPF: u64 = ioc(1, 8, 4);
pub const IOC_PAUSE_OUTPUT: u64 = ioc(1, 9, 4);
pub const IOC_QUERY_BPF: u64 = ioc(3, 10, 8);
pub const IOC_MODIFY_ATTRIBUTES: u64 = ioc(1, 11, 8);
/// `PERF_IOC_FLAG_GROUP`: ENABLE/DISABLE/RESET/REFRESH act on the whole group.
pub const IOC_FLAG_GROUP: u64 = 1;

pub const RECORD_MMAP: u32 = 1;
pub const RECORD_LOST: u32 = 2;
pub const RECORD_COMM: u32 = 3;
pub const RECORD_EXIT: u32 = 4;
pub const RECORD_THROTTLE: u32 = 5;
pub const RECORD_UNTHROTTLE: u32 = 6;
pub const RECORD_FORK: u32 = 7;
pub const RECORD_SAMPLE: u32 = 9;
pub const RECORD_MMAP2: u32 = 10;

pub const MISC_CPUMODE_MASK: u16 = 7;
pub const MISC_KERNEL: u16 = 1;
pub const MISC_USER: u16 = 2;
pub const MISC_MMAP_DATA: u16 = 1 << 13;
pub const MISC_COMM_EXEC: u16 = 1 << 13;

pub const CONTEXT_KERNEL: u64 = -128i64 as u64;
pub const CONTEXT_USER: u64 = -512i64 as u64;
/// `PERF_CONTEXT_MAX`: callchain entries at or above it are markers, not addresses.
pub const CONTEXT_MAX: u64 = -4095i64 as u64;

/// `struct perf_event_mmap_page` offsets (the control page, page 0 of the mapping).
pub mod page {
    pub const VERSION: usize = 0;
    pub const COMPAT_VERSION: usize = 4;
    pub const INDEX: usize = 12;
    pub const OFFSET: usize = 16;
    pub const TIME_ENABLED: usize = 24;
    pub const TIME_RUNNING: usize = 32;
    pub const CAPABILITIES: usize = 40;
    /// `size`: the header up to `__reserved` (the user-page layout Linux fills).
    pub const SIZE: usize = 72;
    pub const SIZE_VALUE: u32 = 96;
    pub const DATA_HEAD: usize = 1024;
    pub const DATA_TAIL: usize = 1032;
    pub const DATA_OFFSET: usize = 1040;
    pub const DATA_SIZE: usize = 1048;
    /// `cap_bit0_is_deprecated` (bit 1 of capabilities): set by every Linux since 3.12.
    pub const CAP_BIT0_IS_DEPRECATED: u64 = 1 << 1;
}

/// Linux's defaults for the sysctls of kernel/events/core.c (CachyOS shows the same).
pub const DEFAULT_PARANOID: i64 = 2;
pub const DEFAULT_MAX_SAMPLE_RATE: u64 = 100_000;
pub const DEFAULT_MAX_STACK: u64 = 127;
pub const DEFAULT_MLOCK_KB: u64 = 516;
/// `sysctl_perf_event_max_contexts_per_stack`.
pub const MAX_CONTEXTS_PER_STACK: u64 = 8;
