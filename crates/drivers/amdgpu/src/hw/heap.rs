//! GX-8a: the carve-out as the hardware backend uses it. Two heaps over whole 2 MiB slabs taken from the device's
//! `BoManager`: the user heap (GEM objects, CPU type write-combined, as Linux maps VRAM) and the table heap (GPU page
//! tables, uncached, one 8-byte store per entry). Slabs are pieces no other memory type touches (the rule of
//! `pspbringup::Pieces`, Intel SDM Vol. 3 11.12.4): the scanout, the PSP, the rings and MES live in other BOs of the
//! manager, so nothing here can hand them out.
//!
//! A slab whose pages are all free goes back to the manager, window first: it leaves the heap, its window is unmapped
//! on every CPU, then the manager may hand the range to anyone (the display, the PSP's uncached pieces). So no two
//! types ever map one page. A small cushion stays to spare the TLB shootdowns of a create/free loop: free object slabs
//! up to the reserve, table slabs up to the reserve. The table heap can always grow to `Heaps::reserve` bytes: there
//! is no eviction here, so a program that fills the carve-out with objects must not leave its next mapping without
//! tables.

use crate::bo::{BoManager, Placement, Reserved};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use amdgpu_fmt::addr::Carveout;
use amdgpu_fmt::store::TABLE_BYTES;
use core::ops::{Deref, Range};
use ksync::{Mutex, SpinLock};

/// A slab's size and alignment: the CPU maps whole pieces.
pub const SLAB: u64 = crate::gpucore::pspbringup::PIECE;
/// The floor of the table reserve (a 4 GiB scattered mapping's tables, 8 MiB, twice).
const RESERVE_MIN: u64 = 16 << 20;

/// What a window of the carve-out holds, which decides its CPU memory type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemKind {
    /// Uncached: every entry reaches memory as one store, in order.
    Tables,
    /// Write-combined: the type programs map GEM objects with.
    User,
}

/// A CPU window over whole pages of the carve-out; offsets are from the window's start. False or None: outside it.
pub trait Window: Send + Sync {
    fn read64(&self, off: u64) -> Option<u64>;
    fn write64(&self, off: u64, value: u64) -> bool;
    fn read(&self, off: u64, out: &mut [u8]) -> bool;
    fn write(&self, off: u64, data: &[u8]) -> bool;
    /// `len` bytes (8-aligned) to one 64-bit value.
    fn fill(&self, off: u64, len: u64, value: u64) -> bool {
        (0..len / 8).all(|i| self.write64(off + i * 8, value))
    }
}

/// How the CPU reaches the carve-out: live windows on the chip, the fake chain's memory in tests.
pub trait CarveIo: Send + Sync {
    /// A window over the carve-out offsets `range`; dropping the last `Arc` unmaps it.
    fn map(&self, range: Range<u64>, kind: MemKind) -> Option<Arc<dyn Window>>;
    /// Every earlier store reached memory (before a TLB flush can follow).
    fn barrier(&self);
    /// Programs can map the carve-out's physical pages (false for a fake's made-up addresses).
    fn mappable(&self) -> bool;
}

/// Where slabs come from: the device's manager.
pub trait SlabSource: Send + Sync {
    /// `bytes` at `align` of free carve-out, until `give`.
    fn take(&self, bytes: u64, align: u64) -> Option<(u32, Range<u64>)>;
    /// What `take` gave under `handle`: no CPU window maps it any more.
    fn give(&self, handle: u32);
    fn free_bytes(&self) -> u64;
}

impl<M: Deref<Target = SpinLock<BoManager>> + Send + Sync> SlabSource for M {
    fn take(&self, bytes: u64, align: u64) -> Option<(u32, Range<u64>)> {
        let mut m = self.lock();
        let h = m.create(bytes, Placement::Vram, align, None).ok()?;
        m.get(h).map(|b| (h, b.range.clone()))
    }
    fn give(&self, handle: u32) {
        self.lock().release(handle);
    }
    fn free_bytes(&self) -> u64 {
        self.lock().free_bytes()
    }
}

/// Part of a range in one slab's window.
#[derive(Clone)]
pub struct Seg {
    pub win: Arc<dyn Window>,
    /// Offset in the window.
    pub at: u64,
    pub len: u64,
}

struct Slab {
    end: u64,
    win: Arc<dyn Window>,
    /// The source's handle.
    handle: u32,
}

struct Heap {
    alloc: BoManager,
    slabs: BTreeMap<u64, Slab>,
    /// Bytes of all slabs.
    bytes: u64,
}

impl Heap {
    /// A manager whose free list holds only what `add` gives it.
    fn new(size: u64) -> Heap {
        let out = Reserved { name: String::from("not in this heap"), range: 0..size };
        Heap { alloc: BoManager::new(size, alloc::vec![out]), slabs: BTreeMap::new(), bytes: 0 }
    }

    fn add(&mut self, r: Range<u64>, win: Arc<dyn Window>, handle: u32) {
        self.bytes += r.end - r.start;
        self.slabs.insert(r.start, Slab { end: r.end, win, handle });
        self.alloc.donate(r);
    }

    /// Slabs `r` touches whose pages are all free leave the heap, while more than `keep_free` bytes are free and
    /// `keep_bytes` bytes of slabs stay.
    fn withdraw(&mut self, r: &Range<u64>, keep_free: u64, keep_bytes: u64) -> Vec<Slab> {
        let mut gone = Vec::new();
        let mut at = self.slabs.range(..=r.start).next_back().map_or(r.start, |(&s, _)| s);
        while at < r.end {
            let Some((&s, slab)) = self.slabs.range(at..r.end).next() else { break };
            let end = slab.end;
            at = end;
            let len = end - s;
            let (free, bytes) = (self.alloc.free_bytes(), self.bytes);
            if end <= r.start || free <= keep_free || bytes.saturating_sub(len) < keep_bytes || gone.try_reserve(1).is_err() {
                continue;
            }
            if self.alloc.withdraw(s..end)
                && let Some(slab) = self.slabs.remove(&s)
            {
                self.bytes -= len;
                gone.push(slab);
            }
        }
        gone
    }

    /// The windows of `r`, which lies in slabs of this heap (neighbouring slabs share one free range).
    fn segments(&self, r: &Range<u64>) -> Option<Vec<Seg>> {
        let mut v = Vec::new();
        let mut at = r.start;
        while at < r.end {
            let (&s, slab) = self.slabs.range(..=at).next_back()?;
            if at >= slab.end {
                return None;
            }
            let len = slab.end.min(r.end) - at;
            v.push(Seg { win: slab.win.clone(), at: at - s, len });
            at += len;
        }
        Some(v)
    }
}

/// A range of the user heap.
pub struct UserRange {
    pub handle: u32,
    pub range: Range<u64>,
    pub segs: Vec<Seg>,
}

/// One page of the table heap.
#[derive(Clone)]
pub struct TablePage {
    pub handle: u32,
    pub offset: u64,
    pub win: Arc<dyn Window>,
    pub at: u64,
}

struct State {
    user: Heap,
    tables: Heap,
}

pub struct Heaps {
    pub carve: Carveout,
    pub io: Arc<dyn CarveIo>,
    source: alloc::boxed::Box<dyn SlabSource>,
    reserve: u64,
    st: Mutex<State>,
}

/// The bytes the backend holds, for INFO.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapBytes {
    pub user_slabs: u64,
    pub user_used: u64,
    pub table_slabs: u64,
    pub table_used: u64,
    /// Free in the manager.
    pub free: u64,
    /// Of `free`, kept back for tables.
    pub reserved: u64,
}

fn used(h: &Heap) -> u64 {
    h.bytes - h.alloc.free_bytes()
}

impl Heaps {
    pub fn new(carve: Carveout, io: Arc<dyn CarveIo>, source: alloc::boxed::Box<dyn SlabSource>) -> Arc<Heaps> {
        let reserve = (carve.size() / 32).max(RESERVE_MIN).next_multiple_of(SLAB);
        let st = State { user: Heap::new(carve.size()), tables: Heap::new(carve.size()) };
        Arc::new(Heaps { carve, io, source, reserve, st: Mutex::named("amdgpu.hw.heaps", st) })
    }

    pub fn bytes(&self) -> HeapBytes {
        let g = self.st.lock();
        let free = self.source.free_bytes();
        HeapBytes {
            user_slabs: g.user.bytes,
            user_used: used(&g.user),
            table_slabs: g.tables.bytes,
            table_used: used(&g.tables),
            free,
            reserved: self.reserve.saturating_sub(g.tables.bytes).min(free),
        }
    }

    /// A whole slab's worth more for `h`: `want` bytes rounded up to slabs.
    fn grow(&self, h: &mut Heap, want: u64, align: u64, kind: MemKind, keep_back: u64) -> bool {
        let Some(bytes) = want.checked_next_multiple_of(SLAB) else { return false };
        if self.source.free_bytes() < bytes.saturating_add(keep_back) {
            klog::event!(Debug, "gx", "hw.heap_full", kind = alloc::format!("{kind:?}").as_str(), bytes = bytes, keep_back = keep_back);
            return false;
        }
        let Some((handle, r)) = self.source.take(bytes, align.max(SLAB)) else { return false };
        // A window the CPU cannot get leaves the slab taken: its type is unknown, so it stays out of every heap.
        let Some(win) = self.io.map(r.clone(), kind) else {
            klog::event!(Error, "gx", "hw.slab_unmapped", start = klog::Hex(r.start), end = klog::Hex(r.end), kind = alloc::format!("{kind:?}").as_str());
            return false;
        };
        klog::event!(Debug, "gx", "hw.slab", start = klog::Hex(r.start), end = klog::Hex(r.end), kind = alloc::format!("{kind:?}").as_str());
        h.add(r, win, handle);
        true
    }

    /// Slabs back to the source, each window unmapped first. No heap lock held: the unmap waits for every CPU.
    fn give_back(&self, gone: Vec<Slab>) {
        for Slab { end, win, handle } in gone {
            drop(win);
            self.source.give(handle);
            klog::event!(Debug, "gx", "hw.slab_back", end = klog::Hex(end));
        }
    }

    /// `bytes` (whole pages) of the user heap at `align` (a power of two, at least a page).
    pub fn alloc_user(&self, bytes: u64, align: u64) -> Option<UserRange> {
        let mut g = self.st.lock();
        let State { user, tables } = &mut *g;
        let keep_back = self.reserve.saturating_sub(tables.bytes);
        let handle = match user.alloc.create(bytes, Placement::Vram, align, None) {
            Ok(h) => h,
            Err(_) => {
                if !self.grow(user, bytes, align, MemKind::User, keep_back) {
                    return None;
                }
                user.alloc.create(bytes, Placement::Vram, align, None).ok()?
            }
        };
        let range = user.alloc.get(handle)?.range.clone();
        let segs = user.segments(&range)?;
        Some(UserRange { handle, range, segs })
    }

    /// The caller's `Seg`s of this range are gone (they hold the window a returning slab must lose first).
    pub fn release_user(&self, handle: u32) {
        let gone = {
            let mut g = self.st.lock();
            let r = g.user.alloc.get(handle).map(|b| b.range.clone());
            g.user.alloc.release(handle);
            r.map_or_else(Vec::new, |r| g.user.withdraw(&r, self.reserve, 0))
        };
        self.give_back(gone);
    }

    /// One page for a table.
    pub fn alloc_table(&self) -> Option<TablePage> {
        let mut g = self.st.lock();
        let t = &mut g.tables;
        let handle = match t.alloc.create(TABLE_BYTES, Placement::Vram, TABLE_BYTES, None) {
            Ok(h) => h,
            Err(_) => {
                if !self.grow(t, SLAB, SLAB, MemKind::Tables, 0) {
                    return None;
                }
                t.alloc.create(TABLE_BYTES, Placement::Vram, TABLE_BYTES, None).ok()?
            }
        };
        let offset = t.alloc.get(handle)?.range.start;
        let (&s, slab) = t.slabs.range(..=offset).next_back()?;
        Some(TablePage { handle, offset, win: slab.win.clone(), at: offset - s })
    }

    pub fn free_table(&self, page: TablePage) {
        let TablePage { handle, offset, win, .. } = page;
        drop(win);
        let gone = {
            let mut g = self.st.lock();
            g.tables.alloc.release(handle);
            g.tables.withdraw(&(offset..offset + TABLE_BYTES), 0, self.reserve)
        };
        self.give_back(gone);
    }

    /// The table reserve, which is also the cushion of free object slabs.
    pub fn reserve(&self) -> u64 {
        self.reserve
    }

    /// Tables that can still be had, at most.
    pub fn table_room(&self) -> usize {
        let free = self.st.lock().tables.alloc.free_bytes();
        (free.saturating_add(self.source.free_bytes()) / TABLE_BYTES) as usize
    }
}
