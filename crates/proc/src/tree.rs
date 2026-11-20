//! EINS-C (K5): a thread group's assistant tree (`policy::tree`). Set once, by the group's own entry or at fork
//! from the parent, kept by execve (the group stays), never cleared: no call takes a process out, and a process
//! in a tree cannot enter another. The chat label travels the same way and only a tree process sets its own.
//!
//! Two ways in: a process puts itself into a fresh tree (`/proc/self/bitsos_tree`, "enter"), and the kernel puts
//! `/bin/assist` in one at its execve when the parent is the compositor (holds the screen's compositor grant, or
//! runs bitscomp's program): bitscomp starts the service `assist serve` that way (B/assist_serve.rs), so the
//! service never runs outside one by mistake.

use crate::{Group, Process};
use alloc::sync::Arc;
use policy::tree::Tree;
use uapi::errno::*;
use vfs::Errno;

/// The service's program; also under /usr/bin when /bin leads there.
const ASSIST: [&str; 2] = ["/bin/assist", "/usr/bin/assist"];
/// The compositor's program (init's `DESKTOP_EXE`, on the stick beside the Mesa tree): a parent running it counts even
/// while it holds no grant (the grant can be refused, and bitscomp then runs on without it).
const COMPOSITOR: [&str; 1] = ["/boot/BITSOS2/bin/bitscomp"];

klog::counter!(ENTERED, "proc.tree.entered");
klog::counter!(REFUSED, "proc.tree.refused");

impl Group {
    /// The tree this group is in.
    pub fn tree(&self) -> Option<&Arc<Tree>> {
        self.tree.get()
    }
    /// Its number (0: none), what `Subject::tree` carries.
    #[inline]
    pub fn tree_id(&self) -> u32 {
        self.tree.get().map_or(0, |t| t.id)
    }
    /// The chat label (None: none set, or not in a tree).
    pub fn chat(&self) -> Option<Arc<str>> {
        self.chat.lock().clone()
    }
    /// Puts this group into a fresh tree, entered by `pid`. EPERM when it is in one already.
    pub fn enter_tree(&self, pid: u32) -> Result<u32, Errno> {
        if self.tree.get().is_some() {
            REFUSED.inc();
            klog::event!(Info, "proc", "tree.enter_refused", pid = pid, tree = self.tree_id(); "already in an assistant tree");
            return Err(EPERM);
        }
        let t = policy::tree::new(pid, time::now_ns()).ok_or(ENOMEM)?;
        let set = self.tree.get_or_init(|| t.clone());
        if !Arc::ptr_eq(set, &t) {
            REFUSED.inc();
            return Err(EPERM);
        }
        ENTERED.inc();
        Ok(t.id)
    }
    /// Sets the chat label (`policy::tree::valid_chat`). Only inside a tree.
    pub fn set_chat(&self, label: &str) -> Result<(), Errno> {
        if self.tree.get().is_none() {
            return Err(EPERM);
        }
        if !policy::tree::valid_chat(label.as_bytes()) {
            return Err(EINVAL);
        }
        let new: Arc<str> = Arc::from(label);
        let old = self.chat.lock().replace(new);
        drop(old);
        Ok(())
    }
    /// fork/clone of `parent`: the child is in the parent's tree, with its chat label (before it runs).
    pub(crate) fn inherit_tree(&self, parent: &Group) {
        if let Some(t) = parent.tree.get() {
            self.tree.get_or_init(|| t.clone());
            let c = parent.chat.lock().clone();
            *self.chat.lock() = c;
        }
    }
}

/// execve rule: `/bin/assist` started by the compositor enters a fresh tree, unless it is in one.
pub(crate) fn on_exec(p: &Arc<Process>) {
    let g = p.group();
    if g.tree.get().is_some() || !ASSIST.contains(&p.path.as_str()) {
        return;
    }
    let parent = g.ppid();
    let granted = parent != 0 && sched::interactive::grants_of(parent as u64) & sched::interactive::Grant::Compositor as u8 != 0;
    if !granted && !crate::find(parent).is_some_and(|q| COMPOSITOR.contains(&q.path.as_str())) {
        return;
    }
    match g.enter_tree(p.pid) {
        Ok(id) => klog::event!(Info, "proc", "tree.assistant", pid = p.pid, parent = parent, tree = id; "the compositor's assistant runs in its own tree"),
        Err(e) => klog::event!(Warn, "proc", "tree.assistant_failed", pid = p.pid, errno = e),
    }
}
