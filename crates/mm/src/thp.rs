//! MM-13: the system-wide switch and numbers of transparent huge pages (Linux THP, mm/huge_memory.c), and how a
//! fault without a free 2 MiB block gets one (`fault_block`).
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
klog::counter!(pub(crate) DIRECT_STALL, "mm.thp.direct_stall");
klog::counter!(pub(crate) DIRECT_ALLOC, "mm.thp.direct_alloc");
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

/// W35 MM-3: Linux's `/sys/kernel/mm/transparent_hugepage/defrag` (`defrag_show`/`defrag_store`,
/// mm/huge_memory.c; `vma_thp_gfp_mask`): what a fault does that finds no free 2 MiB block. Default `madvise`
/// (Linux's, Georg 6.11. decision 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Defrag {
    /// Every fault compacts directly (where it may stall).
    Always = 0,
    /// No fault stalls; kswapd and kcompactd are woken.
    Defer = 1,
    /// MADV_HUGEPAGE regions compact directly; the others wake kswapd and kcompactd.
    DeferMadvise = 2,
    /// MADV_HUGEPAGE regions compact directly; the others never stall and wake kcompactd only (Linux wakes nobody
    /// there and leaves it to khugepaged, which BitsOS has not: without the wake a machine whose memory once
    /// filled with cache would rarely map a huge page again).
    Madvise = 3,
    /// No fault stalls or wakes anyone.
    Never = 4,
}

impl Defrag {
    const ALL: [Defrag; 5] = [Defrag::Always, Defrag::Defer, Defrag::DeferMadvise, Defrag::Madvise, Defrag::Never];

    pub fn name(self) -> &'static str {
        ["always", "defer", "defer+madvise", "madvise", "never"][self as usize]
    }

    /// Linux `sysfs_match_string`: one word, a trailing newline ignored.
    pub fn parse(s: &[u8]) -> Option<Defrag> {
        let s = s.strip_suffix(b"\n").unwrap_or(s);
        Self::ALL.into_iter().find(|m| m.name().as_bytes() == s)
    }
}

static DEFRAG: AtomicU8 = AtomicU8::new(Defrag::Madvise as u8);

pub fn defrag() -> Defrag {
    Defrag::ALL[(DEFRAG.load(Relaxed) as usize).min(4)]
}

pub fn set_defrag(d: Defrag) {
    let old = DEFRAG.swap(d as u8, Relaxed);
    if old != d as u8 {
        klog::event!(Info, "mm", "thp.defrag", mode = d.name());
    }
}

/// `defrag` as Linux shows it: every mode, the active one in brackets ("always defer defer+madvise [madvise] never\n").
pub fn defrag_text() -> alloc::string::String {
    let cur = defrag();
    let mut s = alloc::string::String::new();
    for (i, m) in Defrag::ALL.into_iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        if m == cur {
            s.push('[');
            s.push_str(m.name());
            s.push(']');
        } else {
            s.push_str(m.name());
        }
    }
    s.push('\n');
    s
}

/// A write to `defrag`: Err(()) for a word Linux does not know (EINVAL).
pub fn defrag_store(buf: &[u8]) -> Result<(), ()> {
    set_defrag(Defrag::parse(buf).ok_or(())?);
    Ok(())
}

/// What a fault that finds no free 2 MiB block does besides falling back to 4 KiB pages (Linux 7.x
/// `vma_thp_gfp_mask` by `defrag`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Steps {
    /// Note order 9 for kswapd (`__GFP_KSWAPD_RECLAIM`), which hands it to kcompactd once the count is balanced.
    pub kswapd: bool,
    /// Wake kcompactd for order 9 directly.
    pub kcompactd: bool,
    /// Compact now, in the fault (`__GFP_DIRECT_RECLAIM`).
    pub direct: bool,
}

/// The steps for a fault in a VMA with (`madvised`) or without MADV_HUGEPAGE under the `defrag` mode.
pub fn steps(madvised: bool) -> Steps {
    let (kswapd, kcompactd, direct) = match defrag() {
        Defrag::Always => (false, false, true),
        Defrag::Defer => (true, true, false),
        Defrag::DeferMadvise => (!madvised, !madvised, madvised),
        Defrag::Madvise => (false, !madvised, madvised),
        Defrag::Never => (false, false, false),
    };
    Steps { kswapd, kcompactd, direct }
}

/// A zeroed 2 MiB block for a fault in a VMA with (`madvised`) or without MADV_HUGEPAGE: the free one, else after
/// one bounded direct compaction where `steps` asks for it and the caller may stall (`may_stall`: not under the
/// space's lock; `Buddy::compact_direct` also refuses where reclaim may not run), else None with kswapd or
/// kcompactd asked for a block (counted as a fallback). A direct step that failed or may not be taken here asks
/// kcompactd too.
pub(crate) fn fault_block(madvised: bool, may_stall: bool) -> Option<u64> {
    let kind = mm_core::Kind::User;
    if let Some(f) = mm_core::alloc_zeroed_huge_light(kind) {
        return Some(f);
    }
    let st = steps(madvised);
    // A direct step the caller may not take now (under the space's lock) is left to kcompactd.
    let mut wake = st.kcompactd || (st.direct && !may_stall);
    if st.direct
        && may_stall
        && let Some(b) = mm_core::boot_buddy()
    {
        DIRECT_STALL.inc();
        if b.compact_direct(mm_core::HUGE_ORDER)
            && let Some(f) = mm_core::alloc_zeroed_huge_light(kind)
        {
            DIRECT_ALLOC.inc();
            return Some(f);
        }
        wake = true;
    }
    if st.kswapd {
        crate::reclaim::want_order(mm_core::HUGE_ORDER);
    }
    if wake {
        mm_core::compact::wake_kcompactd(mm_core::HUGE_ORDER);
    }
    FAULT_FALLBACK.inc();
    None
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
            set_defrag(Defrag::Madvise);
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
    fn defrag_modes_choose_the_steps() {
        let _g = locked();
        assert_eq!(defrag(), Defrag::Madvise, "Linux's default");
        assert_eq!(steps(true), Steps { kswapd: false, kcompactd: false, direct: true }, "madvise: MADV_HUGEPAGE compacts directly");
        assert_eq!(steps(false), Steps { kswapd: false, kcompactd: true, direct: false }, "madvise: elsewhere never a stall, kcompactd woken");
        set_defrag(Defrag::Always);
        assert!(steps(false).direct && steps(true).direct);
        set_defrag(Defrag::Defer);
        assert_eq!(steps(true), Steps { kswapd: true, kcompactd: true, direct: false });
        set_defrag(Defrag::DeferMadvise);
        assert_eq!((steps(true).direct, steps(false).direct, steps(false).kswapd), (true, false, true));
        set_defrag(Defrag::Never);
        assert_eq!(steps(true), Steps { kswapd: false, kcompactd: false, direct: false });
        set_defrag(Defrag::Madvise);
    }

    /// CachyOS 7.3.1 (6.11.2026): `always defer [defer+madvise] madvise never` (CachyOS sets defer+madvise; Linux's
    /// and BitsOS's default is madvise).
    #[test]
    fn defrag_reads_and_writes_like_linux() {
        let _g = locked();
        assert_eq!(defrag_text(), "always defer defer+madvise [madvise] never\n");
        assert!(defrag_store(b"defer+madvise\n").is_ok());
        assert_eq!(defrag_text(), "always defer [defer+madvise] madvise never\n");
        for bad in [&b"defer+"[..], b"[madvise]", b"madvise ", b"", b"never\0", &[b'x'; 4096][..]] {
            assert!(defrag_store(bad).is_err(), "{bad:?}");
        }
        assert_eq!(defrag(), Defrag::DeferMadvise, "a refused write changes nothing");
        assert!(defrag_store(b"always").is_ok());
        assert_eq!(defrag_text(), "[always] defer defer+madvise madvise never\n");
        assert!(defrag_store(b"never").is_ok());
        assert_eq!(defrag_text(), "always defer defer+madvise madvise [never]\n");
        set_defrag(Defrag::Madvise);
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
