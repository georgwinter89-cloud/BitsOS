//! A per-file handle table (Linux's `object_idr`, linux-6.6/drivers/gpu/drm/drm_gem.c:376 `idr_alloc(.., 1, 0, ..)`):
//! the lowest free number from 1 on, up to i32::MAX (ENOSPC beyond, as the IDR); grows without a fixed count (R1).

use alloc::collections::{BTreeMap, BTreeSet};
use linux::errno::ENOSPC;
use vfs::Errno;

const MAX: u32 = i32::MAX as u32;

pub struct Handles<T> {
    map: BTreeMap<u32, T>,
    /// Every number below `next` that is not in `map`.
    free: BTreeSet<u32>,
    next: u32,
}

impl<T> Default for Handles<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Handles<T> {
    pub const fn new() -> Self {
        Handles { map: BTreeMap::new(), free: BTreeSet::new(), next: 1 }
    }

    pub fn insert(&mut self, v: T) -> Result<u32, Errno> {
        let h = match self.free.pop_first() {
            Some(h) => h,
            None if self.next <= MAX => {
                self.next += 1;
                self.next - 1
            }
            None => return Err(ENOSPC),
        };
        self.map.insert(h, v);
        Ok(h)
    }

    pub fn get(&self, h: u32) -> Option<&T> {
        self.map.get(&h)
    }

    /// `idr_replace`: `v` under the existing handle `h`, the old value back; `v` back when `h` is not there.
    pub fn replace(&mut self, h: u32, v: T) -> Result<T, T> {
        match self.map.get_mut(&h) {
            Some(slot) => Ok(core::mem::replace(slot, v)),
            None => Err(v),
        }
    }

    pub fn remove(&mut self, h: u32) -> Option<T> {
        let v = self.map.remove(&h)?;
        self.free.insert(h);
        // Keep `free` small: numbers at the top go back to `next`.
        while self.next > 1 && self.free.remove(&(self.next - 1)) {
            self.next -= 1;
        }
        Some(v)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn values(&self) -> impl Iterator<Item = &T> {
        self.map.values()
    }

    /// Takes every entry out (the last close: Linux `drm_gem_release`).
    pub fn drain(&mut self) -> alloc::vec::Vec<(u32, T)> {
        self.free.clear();
        self.next = 1;
        core::mem::take(&mut self.map).into_iter().collect()
    }
}
