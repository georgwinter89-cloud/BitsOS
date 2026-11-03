//! DR-6: `mmap` of the render node at a GEM object's fake offset (linux-6.6/drivers/gpu/drm/drm_gem.c:1095
//! `drm_gem_mmap`): the range must start exactly at an object's offset and fit in it (EINVAL), the object must
//! be one the file has a handle for (EACCES, `drm_vma_node_is_allowed`), with CPU access (EPERM,
//! `amdgpu_gem_object_mmap`); then the exporter's own rule. Pages come from the object's memory.
//!
//! MM's hook asks the file for one `DeviceMap` without the offset (`File::device_map`), so today the file hands
//! out a view of all its objects (`FileGem::device_map`, rebuilt after a handle change). A mapping keeps the
//! memory of every object in the view it was made from until it is unmapped. A file without a mappable object
//! gets a view that refuses every range (EINVAL/EACCES as Linux, not ENODEV). CR-DR6-1 (plan/notes/dr-DR-6.md)
//! passes the offset (`OpenDrm::object_map`): then each mapping holds its own object's memory only.

use crate::dma_buf::Exporter;
use crate::file::DrmFile;
use crate::gem::{Bo, GemShared};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use linux::errno::*;
use mm::device::{Aperture, DeviceMap, DeviceOps, DevicePage};
use vfs::pagecache::PageFault;
use vfs::Errno;

struct Entry {
    start: u64,
    pages: u64,
    aperture: usize,
    cpu: bool,
    mem: Arc<dyn Exporter>,
    /// For the CPU-access note of a fault (weak: a mapping keeps the memory, not the object).
    bo: Weak<Bo>,
}

/// Offsets (in pages) to the objects' memory.
struct OffsetOps {
    /// Sorted by `start`.
    entries: Vec<Entry>,
    gem: Arc<GemShared>,
}

impl OffsetOps {
    fn find(&self, pg: u64) -> Option<&Entry> {
        let i = self.entries.partition_point(|e| e.start <= pg);
        let e = self.entries.get(i.checked_sub(1)?)?;
        (pg - e.start < e.pages).then_some(e)
    }
}

impl DeviceOps for OffsetOps {
    fn check(&self, pgoff: u64, pages: u64, _prot: u64, shared: bool) -> Result<(), i64> {
        match self.entries.binary_search_by_key(&pgoff, |e| e.start) {
            Ok(i) if pages <= self.entries[i].pages => {
                let e = &self.entries[i];
                if !e.cpu {
                    return Err(EPERM);
                }
                e.mem.mmap_check(shared)
            }
            // Not one of the view's: another file's object (EACCES) or nothing (EINVAL).
            _ => Err(if self.gem.exact(pgoff, pages).is_some() { EACCES } else { EINVAL }),
        }
    }
    fn fault(&self, pgoff: u64, _write: bool) -> Result<DevicePage, PageFault> {
        let e = self.find(pgoff).ok_or(PageFault::Bus)?;
        if let Some(b) = e.bo.upgrade() {
            b.cpu_touched();
        }
        Ok(DevicePage { aperture: e.aperture, page: pgoff - e.start, writable: true })
    }
}

fn build(gem: &Arc<GemShared>, bos: &[Arc<Bo>]) -> Result<Option<Arc<DeviceMap>>, Errno> {
    let mut entries = Vec::new();
    let mut aps: Vec<Aperture> = Vec::new();
    entries.try_reserve_exact(bos.len()).map_err(|_| ENOMEM)?;
    aps.try_reserve_exact(bos.len()).map_err(|_| ENOMEM)?;
    for bo in bos {
        let Some(a) = bo.aperture() else { continue };
        entries.push(Entry { start: bo.pgoff(), pages: bo.pages(), aperture: aps.len(), cpu: bo.cpu_access(), mem: bo.memory().clone(), bo: Arc::downgrade(bo) });
        aps.push(a);
    }
    if aps.is_empty() {
        return Ok(None);
    }
    entries.sort_unstable_by_key(|e| e.start);
    let ops = Arc::new(OffsetOps { entries, gem: gem.clone() });
    match DeviceMap::new(ops, aps) {
        Ok(m) => Ok(Some(m)),
        Err(e) => {
            klog::event!(Warn, "drm", "gem.no_mapping", objects = bos.len(), error = alloc::format!("{e:?}"); "GEM objects cannot be mapped by programs");
            Err(ENOMEM)
        }
    }
}

fn objects(file: &DrmFile) -> Result<Vec<Arc<Bo>>, Errno> {
    Ok(file.handle_objects()?.into_iter().filter_map(|o| o.into_bo()).collect())
}

/// The view of a file without a mappable object: `drm_gem_mmap`'s answers still (EINVAL, EACCES for another
/// file's object), not ENODEV. MM wants one aperture per map, so it holds one page no check lets through.
fn refusing(file: &DrmFile, gem: &Arc<GemShared>) -> Option<Arc<DeviceMap>> {
    let page = mm::dma_pages::DmaPages::alloc_for(1, Default::default(), Some(&file.gem.owner)).ok()?;
    DeviceMap::new(Arc::new(OffsetOps { entries: Vec::new(), gem: gem.clone() }), alloc::vec![Aperture::Pages(page)]).ok()
}

/// The file's view for `File::device_map` (None: no object with pages, mmap then answers as for any device).
pub(crate) fn file_map(file: &DrmFile, gem: &Arc<GemShared>) -> Option<Arc<DeviceMap>> {
    let fg = &file.gem;
    let g0 = fg.generation.load(Ordering::Acquire);
    {
        let g = fg.map.lock();
        if g.0 == g0 && g.1.is_some() {
            return g.1.clone();
        }
    }
    let bos = match objects(file) {
        Ok(b) => b,
        Err(_) => {
            klog::event!(Warn, "drm", "gem.view_no_memory", handles = file.handle_count(); "no memory for the mapping view");
            return None;
        }
    };
    let m = match build(gem, &bos).ok()? {
        Some(m) => m,
        None => refusing(file, gem)?,
    };
    // Kept only if no handle changed while it was built (else the next mmap builds again).
    let mut g = fg.map.lock();
    if fg.generation.load(Ordering::Acquire) == g0 {
        *g = (g0, Some(m.clone()));
    }
    Some(m)
}

/// CR-DR6-1: the mapping of the object at `pgoff` for a range of `pages`, with `drm_gem_mmap`'s checks.
pub(crate) fn object_map(file: &DrmFile, gem: &Arc<GemShared>, pgoff: u64, pages: u64) -> Result<Arc<DeviceMap>, Errno> {
    let bo = gem.exact(pgoff, pages).ok_or(EINVAL)?;
    if !file.has_object(Arc::as_ptr(&bo) as *const ()) {
        return Err(EACCES);
    }
    if !bo.cpu_access() {
        return Err(EPERM);
    }
    if let Some(m) = bo.cpu_map.lock().clone() {
        return Ok(m);
    }
    let m = build(gem, core::slice::from_ref(&bo))?.ok_or(EINVAL)?;
    *bo.cpu_map.lock() = Some(m.clone());
    Ok(m)
}

/// Host tests: the view's answers for an mmap at `pgoff` of `pages` and for a fault (what MM would ask).
#[cfg(test)]
pub(crate) fn view_answers(file: &DrmFile, gem: &Arc<GemShared>, pgoff: u64, pages: u64) -> (Result<(), i64>, Option<(usize, u64)>) {
    let bos = objects(file).unwrap_or_default();
    let mut entries = Vec::new();
    for (k, bo) in bos.iter().filter(|b| b.aperture().is_some()).enumerate() {
        entries.push(Entry { start: bo.pgoff(), pages: bo.pages(), aperture: k, cpu: bo.cpu_access(), mem: bo.memory().clone(), bo: Arc::downgrade(bo) });
    }
    entries.sort_unstable_by_key(|e| e.start);
    let ops = OffsetOps { entries, gem: gem.clone() };
    (ops.check(pgoff, pages, 3, true), ops.fault(pgoff, false).ok().map(|p| (p.aperture, p.page)))
}
