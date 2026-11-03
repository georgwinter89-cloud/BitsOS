//! MM-13: the system-wide switch and numbers of transparent huge pages (Linux THP, linux-6.6/mm/huge_memory.c).
//!
//! Mode as Linux's `/sys/kernel/mm/transparent_hugepage/enabled` (`enabled_show`/`enabled_store`, :239/:255):
//! `always` (CachyOS's default and V2's), `madvise` (only VMAs with MADV_HUGEPAGE) or `never`. CONFIG.TXT `thp=`
//! picks it at boot (`configure`), the sysfs file at run time (`store`). What a VMA needs besides the mode
//! (private anonymous, not a stack, no MADV_NOHUGEPAGE, no PR_SET_THP_DISABLE, a whole aligned 2 MiB inside) is
//! the address space's business (`aspace/huge.rs`).
//!
//! AnonHugePages of /proc/meminfo counts the 2 MiB blocks programs map now that were never split (Linux
//! `NR_ANON_THPS`): +1 when a fault maps one or a split one is merged back, -1 when it is freed whole or split
//! (a partial change, or a fork that shares it 4 KiB-wise).

use core::sync::atomic::{AtomicU64, AtomicU8, Ordering::Relaxed};

/// Bytes of a transparent huge page (`hpage_pmd_size`).
pub const HPAGE_PMD_SIZE: u64 = 2 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Always = 0,
    Madvise = 1,
    Never = 2,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Always => "always",
            Mode::Madvise => "madvise",
            Mode::Never => "never",
        }
    }

    /// The word of `enabled` (a trailing newline is ignored, as Linux's `sysfs_streq`).
    pub fn parse(s: &[u8]) -> Option<Mode> {
        let s = s.strip_suffix(b"\n").unwrap_or(s);
        [Mode::Always, Mode::Madvise, Mode::Never].into_iter().find(|m| m.name().as_bytes() == s)
    }
}

static MODE: AtomicU8 = AtomicU8::new(Mode::Always as u8);
/// 2 MiB blocks mapped and never split (AnonHugePages of /proc/meminfo).
static BLOCKS: AtomicU64 = AtomicU64::new(0);

klog::counter!(pub(crate) FAULT_ALLOC, "mm.thp.fault_alloc");
klog::counter!(pub(crate) FAULT_FALLBACK, "mm.thp.fault_fallback");
klog::counter!(pub(crate) SPLIT_PMD, "mm.thp.split_pmd");
klog::counter!(pub(crate) SPLIT_PAGE, "mm.thp.split_page");
klog::counter!(pub(crate) MERGED, "mm.thp.merged");
klog::counter!(pub(crate) COW_REUSE, "mm.thp.cow_reuse");
klog::counter!(pub(crate) COW_SPLIT, "mm.thp.cow_split");
klog::counter!(pub(crate) FORK_SHARED, "mm.thp.fork_shared");
klog::counter!(pub(crate) UNUSED, "mm.thp.unused");
klog::counter!(pub(crate) SLOT_BUSY, "mm.thp.slot_busy");

pub fn mode() -> Mode {
    match MODE.load(Relaxed) {
        0 => Mode::Always,
        1 => Mode::Madvise,
        _ => Mode::Never,
    }
}

pub fn set_mode(m: Mode) {
    let old = mode();
    MODE.store(m as u8, Relaxed);
    if old != m {
        klog::event!(Info, "mm", "thp.mode", mode = m.name(), was = old.name());
    }
}

/// Reads CONFIG `thp=always|madvise|never` (default always; the kernel hands its getter over at boot).
pub fn configure(get: &dyn Fn(&str) -> Option<alloc::string::String>) {
    let Some(v) = get("thp") else { return };
    match Mode::parse(v.trim().as_bytes()) {
        Some(m) => set_mode(m),
        None => klog::event!(Warn, "mm", "config.unknown", key = "thp", value = v; "thp= takes always|madvise|never; always kept"),
    }
}

/// May a VMA with these MADV_HUGEPAGE / MADV_NOHUGEPAGE flags get huge pages under the mode (Linux
/// `hugepage_vma_check`, huge_memory.c:74: VM_NOHUGEPAGE always refuses, `madvise` wants VM_HUGEPAGE)?
pub fn mode_allows(hugepage: bool, nohugepage: bool) -> bool {
    !nohugepage
        && match mode() {
            Mode::Always => true,
            Mode::Madvise => hugepage,
            Mode::Never => false,
        }
}

/// `/sys/kernel/mm/transparent_hugepage/enabled`: "[always] madvise never\n" with the mode in brackets.
pub fn enabled_text() -> &'static str {
    match mode() {
        Mode::Always => "[always] madvise never\n",
        Mode::Madvise => "always [madvise] never\n",
        Mode::Never => "always madvise [never]\n",
    }
}

/// A write to `enabled`: the new mode, or Err(()) for anything else (EINVAL, as Linux).
pub fn store(buf: &[u8]) -> Result<(), ()> {
    set_mode(Mode::parse(buf).ok_or(())?);
    Ok(())
}

/// `/sys/kernel/mm/transparent_hugepage/hpage_pmd_size` (glibc and jemalloc read it).
pub fn hpage_pmd_size_text() -> &'static str {
    "2097152\n"
}

/// AnonHugePages of /proc/meminfo in bytes.
pub fn anon_huge_bytes() -> u64 {
    BLOCKS.load(Relaxed) * HPAGE_PMD_SIZE
}

pub(crate) fn block_mapped() {
    BLOCKS.fetch_add(1, Relaxed);
}

pub(crate) fn block_gone() {
    if BLOCKS.try_update(Relaxed, Relaxed, |n| n.checked_sub(1)).is_err() {
        klog::event!(Warn, "mm", "thp.blocks_underflow"; "a 2 MiB block went that was never counted: AnonHugePages stays 0");
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    /// MODE is process-wide: every host test that reads or writes it holds this lock and leaves `always` behind.
    static MODE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct Always<'a>(#[allow(dead_code)] std::sync::MutexGuard<'a, ()>);

    impl Drop for Always<'_> {
        fn drop(&mut self) {
            set_mode(Mode::Always);
        }
    }

    fn locked() -> Always<'static> {
        let g = MODE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_mode(Mode::Always);
        Always(g)
    }

    #[test]
    fn parse_like_sysfs_streq() {
        assert_eq!(Mode::parse(b"always\n"), Some(Mode::Always));
        assert_eq!(Mode::parse(b"madvise"), Some(Mode::Madvise));
        assert_eq!(Mode::parse(b"never\n"), Some(Mode::Never));
        assert_eq!(Mode::parse(b"always\n\n"), None, "one newline only, as sysfs_streq");
        assert_eq!(Mode::parse(b""), None);
        assert_eq!(Mode::parse(b"\n"), None);
    }

    #[test]
    fn parse_refuses_hostile_input() {
        let big = [b'a'; 8192];
        for bad in [
            &b"[always]"[..],
            b"[always] madvise never\n",
            b"always\0",
            b"alw\0ays",
            b"\0",
            b" always",
            b"always ",
            b"ALWAYS",
            b"defer",
            b"always\r\n",
            b"\xff\xfe",
            &big[..],
        ] {
            assert_eq!(Mode::parse(bad), None, "{bad:?}");
        }
        let mut long = std::vec::Vec::from(&b"never"[..]);
        long.resize(4096, b'\n');
        assert_eq!(Mode::parse(&long), None, "a page of newlines");
    }

    #[test]
    fn enabled_reads_and_writes_like_linux() {
        let _g = locked();
        assert_eq!(enabled_text(), "[always] madvise never\n", "the default is CachyOS's");
        assert!(mode_allows(false, false) && !mode_allows(true, true), "NOHUGEPAGE always refuses");
        for bad in [&b"defer"[..], b"[always]", b"never\0", b"", &[b'x'; 4096][..]] {
            assert!(store(bad).is_err(), "{bad:?}");
            assert_eq!(mode(), Mode::Always, "a refused write changes nothing");
        }
        assert!(store(b"madvise\n").is_ok());
        assert_eq!(enabled_text(), "always [madvise] never\n");
        assert!(!mode_allows(false, false) && mode_allows(true, false), "madvise wants MADV_HUGEPAGE");
        assert!(store(b"never").is_ok());
        assert!(!mode_allows(true, false));
        assert!(store(b"always").is_ok());
        assert_eq!(hpage_pmd_size_text().trim().parse::<u64>(), Ok(HPAGE_PMD_SIZE));
    }

    #[test]
    fn configure_reads_the_key() {
        let _g = locked();
        configure(&|k| (k == "thp").then(|| alloc::string::String::from(" madvise ")));
        assert_eq!(mode(), Mode::Madvise);
        configure(&|_| Some(alloc::string::String::from("sometimes")));
        assert_eq!(mode(), Mode::Madvise, "an unknown value keeps the mode");
        configure(&|_| None);
        assert_eq!(mode(), Mode::Madvise, "no key keeps the mode");
    }

    #[test]
    fn blocks_never_go_below_zero() {
        let _g = locked();
        block_mapped();
        let n = anon_huge_bytes();
        block_gone();
        assert_eq!(anon_huge_bytes() + HPAGE_PMD_SIZE, n);
        while anon_huge_bytes() > 0 {
            block_gone();
        }
        block_gone();
        assert_eq!(anon_huge_bytes(), 0);
    }
}
