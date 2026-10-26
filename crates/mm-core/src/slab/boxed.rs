//! `SlabBox<T>`: a `Box` whose memory comes from a named `SlabCache`, so crates without `unsafe` can keep
//! their objects in their own cache (and under their own `Kind`).

use super::SlabCache;
use core::ops::{Deref, DerefMut};
use core::ptr::NonNull;

pub struct SlabBox<T> {
    ptr: NonNull<T>,
    cache: &'static SlabCache,
}

// SAFETY: a SlabBox owns its T exclusively, like Box<T>.
unsafe impl<T: Send> Send for SlabBox<T> {}
// SAFETY: as Box<T>: shared access only hands out &T.
unsafe impl<T: Sync> Sync for SlabBox<T> {}

impl<T> SlabBox<T> {
    /// Moves `v` into an object of `cache`. Gives `v` back when memory is out or the cache's objects are too
    /// small or not aligned enough for a `T`.
    pub fn new(cache: &'static SlabCache, v: T) -> Result<SlabBox<T>, T> {
        if size_of::<T>() > cache.object_size() || align_of::<T>() > cache.object_align() {
            return Err(v);
        }
        let Some(p) = cache.alloc() else { return Err(v) };
        let ptr = p.cast::<T>();
        // SAFETY: a fresh object of at least size_of::<T>() bytes, aligned for T (checked above), owned by us.
        unsafe { ptr.as_ptr().write(v) };
        Ok(SlabBox { ptr, cache })
    }

    /// Moves the value out and gives the object back.
    pub fn into_inner(self) -> T {
        let me = core::mem::ManuallyDrop::new(self);
        // SAFETY: the object holds an initialised T we own; it is read once and the memory freed right after,
        // without dropping the T in place (ManuallyDrop keeps `drop` from running).
        let v = unsafe { me.ptr.as_ptr().read() };
        // SAFETY: the pointer came from `me.cache.alloc()` and is freed exactly once.
        let _ = unsafe { me.cache.free(me.ptr.cast()) };
        v
    }
}

impl<T> Deref for SlabBox<T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the object holds an initialised T for the box's life.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T> DerefMut for SlabBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as above; &mut self gives exclusive access.
        unsafe { self.ptr.as_mut() }
    }
}

impl<T> Drop for SlabBox<T> {
    fn drop(&mut self) {
        // SAFETY: the T is initialised and dropped once here; then the object (from `cache.alloc`) is freed
        // once and never used again.
        unsafe {
            core::ptr::drop_in_place(self.ptr.as_ptr());
            let _ = self.cache.free(self.ptr.cast());
        }
    }
}
