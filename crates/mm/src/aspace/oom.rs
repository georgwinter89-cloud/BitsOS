//! MEMCRASH row 2: the OOM reaper's part of an address space (linux-7.3.1/mm/oom_kill.c:518 `__oom_reap_task_mm`):
//! the private memory of a program the OOM killer ended goes back before its threads have left.

use super::{AsCore, Owner, Zap};
use crate::vma::VmaKind;
use hal::paging::tlb;

impl AsCore {
    /// Takes every private page of the space (anonymous memory and the private copies of file mappings; shared
    /// and device mappings stay, as Linux skips VM_SHARED and VM_PFNMAP), keeping the VMAs. Only when `mm.as` is
    /// free (Linux `mmap_read_trylock`): a holder may be the very allocation that waits for this memory. None
    /// then; else the resident pages that went.
    pub fn oom_reap(&self) -> Option<u64> {
        let mut g = self.inner.try_lock()?;
        let before = self.rss_pages();
        let mut z = Zap::begin(self, &g, tlb::Cause::Madvise);
        let mut at = 0;
        while let Some(v) = g.space.tree.next_from(at).cloned() {
            at = v.end;
            if v.file.as_ref().is_some_and(|f| f.shared || f.device.is_some()) {
                continue;
            }
            // One VMA is one piece: its owner without `segments` (a list on the heap; the reaper allocates nothing
            // on this path, as Linux's `__oom_reap_task_mm`). The anonymous zap after it takes what is left.
            if let Some(f) = &v.file {
                let owner = Owner::File(f.backing.clone(), v.kind == VmaKind::Anonymous);
                self.zap(&mut g, &mut z, v.start, v.end, &owner, (0, 0));
            }
            self.zap(&mut g, &mut z, v.start, v.end, &Owner::Anon, (0, 0));
        }
        self.zap_end(&mut g, z);
        Some(before.saturating_sub(self.rss_pages()))
    }
}
