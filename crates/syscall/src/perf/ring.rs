//! The perf ring buffer (linux-7.3.1/kernel/events/ring_buffer.c): a control page (`perf_event_mmap_page`) and
//! 2^n data pages, pinned pages a program maps (`mm::device`), written by the kernel at `data_head`, read by the
//! program up to `data_tail`, which it writes. A record that does not fit counts as lost; the next one that fits
//! is preceded by a PERF_RECORD_LOST. Writers come from the sampling interrupt and from threads (side-band
//! records), so the writer state sits under an IRQ-safe lock.

use super::LOST;
use alloc::sync::Arc;
use alloc::vec;
use core::sync::atomic::{AtomicU32, AtomicU8, Ordering, fence};
use ksync::IrqSpinLock;
use mm::device::{Aperture, DeviceMap, DeviceOps, DevicePage};
use mm::dma_pages::{DmaPages, DmaPagesOptions};
use perf_abi::page;
use perf_abi::record::{self, Ident};
use uapi::errno::*;
use vfs::pagecache::PageFault;
use vfs::poll::{Events, PollQueue};
use alloc::vec::Vec;

const PAGE: u64 = 4096;

/// The pages as programs map them: page i of the mapping is page i of the buffer.
struct Pages {
    pages: u64,
    /// How the first mapping was made (`MODE_*`): its PROT_WRITE decides `rb->overwrite`.
    mode: Arc<AtomicU8>,
}

const MODE_UNSET: u8 = 0;
const MODE_WRITABLE: u8 = 1;
const MODE_READ_ONLY: u8 = 2;
const PROT_WRITE: u64 = 2;

impl DeviceOps for Pages {
    /// `perf_mmap`: offset 0 (no AUX area), the whole buffer, MAP_SHARED.
    fn check(&self, pgoff: u64, pages: u64, prot: u64, shared: bool) -> Result<(), i64> {
        if !shared {
            return Err(EINVAL);
        }
        if pgoff != 0 || pages != self.pages {
            return Err(EINVAL);
        }
        let m = if prot & PROT_WRITE != 0 { MODE_WRITABLE } else { MODE_READ_ONLY };
        let _ = self.mode.compare_exchange(MODE_UNSET, m, Ordering::AcqRel, Ordering::Acquire);
        Ok(())
    }
    /// Only the control page is writable (`perf_mmap_pfn_mkwrite`): a store to a data page is SIGBUS.
    fn fault(&self, pgoff: u64, write: bool) -> Result<DevicePage, PageFault> {
        if pgoff >= self.pages || (write && pgoff != 0) {
            return Err(PageFault::Bus);
        }
        Ok(DevicePage { aperture: 0, page: pgoff, writable: pgoff == 0 })
    }
}

/// What the writer keeps (`struct perf_buffer`'s counters).
struct Writer {
    head: u64,
    /// Records lost since the last LOST record.
    lost: u64,
    /// `rb->wakeup`: the head position of the last wake-up mark.
    wakeup: u64,
    /// Samples since the last wake-up (`wakeup_events`).
    events: u32,
    /// PERF_EVENT_IOC_PAUSE_OUTPUT.
    paused: bool,
}

/// Who pays the buffer's locked memory (`perf_mmap_account`), given back when the event is closed.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Charge {
    pub uid: u32,
    pub user_pages: u64,
}

pub(crate) struct Ring {
    pages: Arc<DmaPages>,
    pub(crate) map: Arc<DeviceMap>,
    /// Pages of the mapping (1 + data pages).
    pub(crate) nr_pages: u64,
    /// Bytes of the data area.
    size: u64,
    watermark: u64,
    w: IrqSpinLock<Writer>,
    /// `rb->poll`: EPOLLIN waits for the next poll since a wake-up.
    poll: AtomicU32,
    /// The first mapping's kind: read-only means the writer does not wait for the reader (`rb->overwrite`).
    mode: Arc<AtomicU8>,
    /// The wait queues of the events writing here (`rb->event_list`); changed in thread context only.
    waiters: IrqSpinLock<Vec<Arc<PollQueue>>>,
}

/// How a record is written: the event's id and trailer (for a LOST record in front of it) and its wake-up rule.
pub(crate) struct Put<'a> {
    pub sample_type: u64,
    pub id_all: bool,
    pub ident: &'a Ident,
    /// A sample (counts for `wakeup_events`).
    pub sample: bool,
    /// `attr.wakeup_events` (0: the watermark decides); ignored with `attr.watermark`.
    pub wakeup_events: u32,
}

impl Ring {
    /// `rb_alloc` for a mapping of `nr_pages` pages (1 + a power of two, or 1 alone); `watermark` 0 = half the area.
    pub(crate) fn new(nr_pages: u64, watermark: u64) -> Result<Arc<Ring>, i64> {
        let data = nr_pages - 1;
        let pages = DmaPages::alloc(nr_pages, DmaPagesOptions::default()).map_err(|_| ENOMEM)?;
        let mode = Arc::new(AtomicU8::new(MODE_UNSET));
        let map = DeviceMap::new(Arc::new(Pages { pages: nr_pages, mode: mode.clone() }), vec![Aperture::Pages(pages.clone())]).map_err(|_| ENOMEM)?;
        let size = data * PAGE;
        let wm = if watermark != 0 { watermark.min(size) } else { perf_abi::ring::default_watermark(size) };
        let r = Ring {
            pages,
            map,
            nr_pages,
            size,
            watermark: wm,
            w: IrqSpinLock::named("perf.ring", Writer { head: 0, lost: 0, wakeup: 0, events: 0, paused: false }),
            mode,
            poll: AtomicU32::new(0),
            waiters: IrqSpinLock::named("perf.ring_waiters", Vec::new()),
        };
        // `perf_event_init_userpage`.
        r.put32(page::VERSION, 0);
        r.put32(page::COMPAT_VERSION, 0);
        r.put64(page::CAPABILITIES, page::CAP_BIT0_IS_DEPRECATED);
        r.put32(page::SIZE, page::SIZE_VALUE);
        r.put64(page::DATA_OFFSET, PAGE);
        r.put64(page::DATA_SIZE, size);
        Ok(Arc::new(r))
    }

    fn put32(&self, off: usize, v: u32) {
        self.pages.write_u32(off as u64, v);
    }
    fn put64(&self, off: usize, v: u64) {
        self.pages.write_u64(off as u64, v);
    }

    pub(crate) fn pause(&self, on: bool) {
        self.w.lock().paused = on;
    }

    /// `perf_event_update_userpage`: count and times for a program reading them without a syscall (`lock` is the
    /// sequence count around them; `index` 0 = no rdpmc).
    pub(crate) fn update_userpage(&self, count: u64, enabled: u64, running: u64) {
        let _w = self.w.lock();
        let seq = self.pages.read_u32(8).unwrap_or(0);
        self.put32(8, seq.wrapping_add(1));
        fence(Ordering::Release);
        self.put32(page::INDEX, 0);
        self.put64(page::OFFSET, count);
        self.put64(page::TIME_ENABLED, enabled);
        self.put64(page::TIME_RUNNING, running);
        fence(Ordering::Release);
        self.put32(8, seq.wrapping_add(2));
    }

    /// Writes `rec` (a whole record); true when readers should be woken. Lost records are counted here and in
    /// `LOST`; the event's own count is the caller's (`lost_samples`).
    pub(crate) fn put(&self, rec: &[u8], p: &Put<'_>) -> Result<bool, ()> {
        let mut w = self.w.lock();
        if w.paused || self.size == 0 {
            if self.size != 0 {
                w.lost += 1;
                LOST.inc();
            }
            return Err(());
        }
        let mut lost_buf = [0u8; 96];
        let lost = if w.lost != 0 { record::lost(&mut lost_buf, p.sample_type, p.id_all, p.ident, w.lost) } else { None };
        let need = rec.len() as u64 + lost.map_or(0, |l| l.len() as u64);
        let tail = self.pages.read_u64(page::DATA_TAIL as u64).unwrap_or(0);
        fence(Ordering::Acquire);
        let overwrite = self.mode.load(Ordering::Relaxed) == MODE_READ_ONLY;
        if !overwrite && perf_abi::ring::space(w.head, tail, self.size) < need {
            w.lost += 1;
            LOST.inc();
            return Err(());
        }
        let mut at = w.head;
        if let Some(l) = lost {
            self.copy_in(at, l);
            at += l.len() as u64;
            w.lost = 0;
        }
        self.copy_in(at, rec);
        w.head = at + rec.len() as u64;
        // The data before the head (Linux `perf_output_put_handle`: smp_wmb, then data_head).
        fence(Ordering::Release);
        self.put64(page::DATA_HEAD, w.head);
        let mut wake = false;
        if w.head - w.wakeup > self.watermark {
            w.wakeup += self.watermark;
            wake = true;
        }
        if p.sample && p.wakeup_events != 0 {
            w.events += 1;
            if w.events >= p.wakeup_events {
                w.events -= p.wakeup_events;
                wake = true;
            }
        }
        if wake {
            self.poll.store(Events::IN | Events::RDNORM, Ordering::Release);
        }
        Ok(wake)
    }

    fn copy_in(&self, pos: u64, b: &[u8]) {
        let (off, first, rest) = perf_abi::ring::pieces(pos, b.len() as u64, self.size);
        self.pages.write(PAGE + off, &b[..first as usize]);
        if rest != 0 {
            self.pages.write(PAGE, &b[first as usize..]);
        }
    }

    /// An event writing here wants the wake-ups. The list is rebuilt outside the IRQ-safe lock and swapped in
    /// (the sampler never allocates; nothing allocates with interrupts off).
    pub(crate) fn add_waiter(&self, q: Arc<PollQueue>) -> Result<(), i64> {
        loop {
            let n = self.waiters.lock().len();
            let mut v: Vec<Arc<PollQueue>> = Vec::new();
            v.try_reserve_exact(n + 1).map_err(|_| ENOMEM)?;
            let old = {
                let mut w = self.waiters.lock();
                if w.len() != n {
                    None
                } else {
                    v.extend(w.iter().cloned());
                    v.push(q.clone());
                    Some(core::mem::replace(&mut *w, v))
                }
            };
            if old.is_some() {
                drop(old);
                return Ok(());
            }
        }
    }

    /// The event stops writing here (its queue goes outside the lock).
    pub(crate) fn remove_waiter(&self, q: &Arc<PollQueue>) {
        let gone = {
            let mut w = self.waiters.lock();
            w.iter().position(|x| Arc::ptr_eq(x, q)).map(|i| w.swap_remove(i))
        };
        drop(gone);
    }

    /// `ring_buffer_wakeup`: every event writing here (IRQ-safe, no allocation).
    pub(crate) fn wake_all(&self) {
        for q in self.waiters.lock().iter() {
            q.wake_poll(Events::IN | Events::RDNORM);
        }
    }

    /// `atomic_xchg(&rb->poll, 0)`: what poll reports, once.
    pub(crate) fn take_poll(&self) -> u32 {
        self.poll.swap(0, Ordering::AcqRel)
    }

    /// The head as the kernel last wrote it (ktests).
    pub(crate) fn head(&self) -> u64 {
        self.w.lock().head
    }

    /// Reads `out.len()` bytes of the data area at stream position `pos` (ktests, wrap undone).
    pub(crate) fn read_data(&self, pos: u64, out: &mut [u8]) {
        let (off, first, rest) = perf_abi::ring::pieces(pos, out.len() as u64, self.size);
        self.pages.read(PAGE + off, &mut out[..first as usize]);
        if rest != 0 {
            self.pages.read(PAGE, &mut out[first as usize..]);
        }
    }

    /// Writes `data_tail` as a reader would (ktests).
    pub(crate) fn set_tail(&self, tail: u64) {
        self.put64(page::DATA_TAIL, tail);
    }

    pub(crate) fn data_size(&self) -> u64 {
        self.size
    }

    pub(crate) fn page_u64(&self, off: usize) -> u64 {
        self.pages.read_u64(off as u64).unwrap_or(0)
    }
}
