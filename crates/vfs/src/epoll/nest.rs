//! The nesting checks of `epoll_ctl` (linux-6.6/fs/eventpoll.c `ep_loop_check` :2010, `reverse_path_check`
//! :1387): run under the global `NEST` mutex, before (loop) and after (paths) the new item is linked. They take
//! each epoll's item mutex one at a time, never two at once.

use super::{EpInner, EpItem, EP_MAX_NESTS};
use crate::fd::OpenFile;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

/// `path_limits`: wake-up paths per depth a file may have through nested epolls (`reverse_path_check`).
const PATH_LIMITS: [u32; 5] = [1000, 500, 100, 50, 10];

/// `ep_loop_check` as Linux has it since the fix of unbounded recursion (CVE-2025-38614, in CachyOS's 7.2 and
/// the later 6.6 stable kernels; the 6.6.0 reference counted only the depth below `to`): no path below `to`
/// may lead back to `ep`, and the longest chain through the new link (below `to` + 1 + above `ep`) may not be
/// longer than `EP_MAX_NESTS`. Files (not epolls) met below are listed for the path check.
pub(super) fn loop_check(ep: &Arc<EpInner>, to: &Arc<EpInner>, visit: u64, files: &mut Vec<Arc<OpenFile>>) -> Result<(), ()> {
    let down = loop_check_down(to, ep, visit, 0, files);
    if down > EP_MAX_NESTS {
        return Err(());
    }
    let up = depth_up(ep, visit, 0);
    if down + 1 + up > EP_MAX_NESTS { Err(()) } else { Ok(()) }
}

/// `ep_loop_check_proc`: the depth of the epolls below `ep` (u32::MAX: a loop back to `inserting`, or deeper
/// than allowed). The visit mark makes each epoll count once per check.
fn loop_check_down(ep: &Arc<EpInner>, inserting: &Arc<EpInner>, visit: u64, depth: u32, files: &mut Vec<Arc<OpenFile>>) -> u32 {
    if ep.visit.load(Ordering::SeqCst) == visit {
        return ep.depth.load(Ordering::SeqCst);
    }
    ep.visit.store(visit, Ordering::SeqCst);
    let below: Vec<Arc<EpItem>> = {
        let items = ep.items.lock();
        let mut v = Vec::new();
        if v.try_reserve_exact(items.len()).is_err() {
            return u32::MAX;
        }
        v.extend(items.iter().cloned());
        v
    };
    let mut result = 0u32;
    for it in &below {
        if let Some(t) = it.file.as_epoll() {
            result = if Arc::ptr_eq(&t.inner, inserting) || depth > EP_MAX_NESTS {
                u32::MAX
            } else {
                result.max(loop_check_down(&t.inner, inserting, visit, depth + 1, files).saturating_add(1))
            };
            if result > EP_MAX_NESTS {
                break;
            }
        } else if let Some(of) = it.owner.upgrade()
            && !files.iter().any(|f| Arc::ptr_eq(f, &of))
        {
            if files.try_reserve(1).is_err() {
                return u32::MAX;
            }
            files.push(of);
        }
    }
    ep.depth.store(result, Ordering::SeqCst);
    result
}

/// `ep_get_upwards_depth_proc`: how many epolls watch `ep` above it, along the longest path.
fn depth_up(ep: &Arc<EpInner>, visit: u64, depth: u32) -> u32 {
    if ep.visit.load(Ordering::SeqCst) == visit {
        return ep.depth.load(Ordering::SeqCst);
    }
    let mut result = 0u32;
    // The graph has no loops and is at most EP_MAX_NESTS deep (both checked at every insertion); the bound
    // here only guards the stack against a broken invariant.
    if depth <= EP_MAX_NESTS + 1 {
        let up = ep.file.lock().upgrade();
        if let Some(of) = &up {
            for w in of.ep_links() {
                if let Some(it) = w.upgrade()
                    && !it.dead.load(Ordering::SeqCst)
                {
                    result = result.max(depth_up(&it.ep, visit, depth + 1).saturating_add(1));
                }
            }
        }
        drop(up);
    }
    ep.visit.store(visit, Ordering::SeqCst);
    ep.depth.store(result, Ordering::SeqCst);
    result
}

/// `reverse_path_check`: from each listed file up through the epolls watching it, no path deeper than
/// `EP_MAX_NESTS` and no more paths per depth than `PATH_LIMITS`.
pub(super) fn reverse_path_check(files: &[Arc<OpenFile>], keep: &mut Vec<Arc<OpenFile>>) -> Result<(), ()> {
    for f in files {
        let mut counts = [0u32; PATH_LIMITS.len()];
        reverse_proc(f.ep_links(), 0, &mut counts, keep)?;
    }
    Ok(())
}

fn reverse_proc(links: Vec<Weak<EpItem>>, depth: u32, counts: &mut [u32; 5], keep: &mut Vec<Arc<OpenFile>>) -> Result<(), ()> {
    if depth > EP_MAX_NESTS {
        return Err(());
    }
    for w in links {
        let Some(it) = w.upgrade() else { continue };
        if it.dead.load(Ordering::SeqCst) {
            continue;
        }
        let up = it.ep.file.lock().upgrade();
        let refs = up.as_ref().map(|of| of.ep_links()).unwrap_or_default();
        if let Some(of) = up {
            let _ = keep.try_reserve(1);
            keep.push(of);
        }
        let live = refs.iter().any(|w| w.upgrade().is_some_and(|x| !x.dead.load(Ordering::SeqCst)));
        if !live {
            // path_count_inc
            if depth > 0 {
                let c = &mut counts[depth as usize];
                *c += 1;
                if *c > PATH_LIMITS[depth as usize] {
                    return Err(());
                }
            }
        } else {
            reverse_proc(refs, depth + 1, counts, keep)?;
        }
    }
    Ok(())
}
