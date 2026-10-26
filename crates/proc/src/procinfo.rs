//! AB-7a: what `/proc/<pid>` needs from the process model that no syscall keeps: the image's layout (Linux
//! `mm->{start_code, end_code, start_data, end_data, start_stack, arg_start, arg_end, env_start, env_end}` and
//! `mm->exe_file`, set by linux-6.6/fs/binfmt_elf.c `load_elf_binary` :1193-1300 and `create_elf_tables`
//! :335-362), the signal sets `/proc/<pid>/status` prints (fs/proc/array.c `task_sig`), and `oom_score_adj`
//! (fs/proc/base.c `__set_oom_adj` :1060; V2 has no OOM killer yet, the value is kept and inherited).

use crate::elf::{Elf, PAGE};
use crate::group::Group;
use crate::signal::{SIG_DFL, SIG_IGN};
use crate::stack;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use uapi::errno::*;
use vfs::Errno;

/// One PT_LOAD as `/proc/<pid>/maps` shows it: `[start, file_end)` maps the file from page offset `off`,
/// `[file_end, end)` is the anonymous rest (`.bss` past the file's last page).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImageSeg {
    pub start: u64,
    pub file_end: u64,
    pub end: u64,
    pub off: u64,
}

/// The layout of one program image (kept per `Process`, copied by fork, made new by execve).
#[derive(Clone, Default)]
pub struct ImageInfo {
    /// The executable (Linux `mm->exe_file`): None for images started from memory (ktests).
    pub exe: Option<vfs::Path>,
    /// The exe's `deny_write_access` (Linux keeps it with `mm->exe_file`): opens for writing and truncate get
    /// ETXTBSY while a process runs the file. Shared by fork; dropped at exec, at the end of the last thread
    /// (`take_exe_deny`) and with the process. AB-8 CR2.
    pub deny: Option<alloc::sync::Arc<vfs::inode::DenyWrite>>,
    /// Made by fork/clone and not exec'd since (Linux PF_FORKNOEXEC).
    pub forked: bool,
    pub start_code: u64,
    pub end_code: u64,
    pub start_data: u64,
    pub end_data: u64,
    pub start_stack: u64,
    pub arg_start: u64,
    pub arg_end: u64,
    pub env_start: u64,
    pub env_end: u64,
    pub segs: Vec<ImageSeg>,
}

impl ImageInfo {
    /// Linux's rules over the PT_LOADs (`load_elf_binary`): start_code = lowest PF_X vaddr, end_code = highest
    /// PF_X vaddr + filesz, start_data = highest vaddr, end_data = highest vaddr + filesz, all plus the bias;
    /// the strings' ranges from the stack the loader built.
    pub(crate) fn of_load(elf: &Elf, bias: u64, img: &stack::Image) -> ImageInfo {
        let mut i = ImageInfo { start_code: u64::MAX, ..Default::default() };
        for l in &elf.loads {
            let k = l.vaddr.wrapping_add(bias);
            let kf = k.wrapping_add(l.filesz);
            if l.flags & crate::elf::PF_X != 0 {
                i.start_code = i.start_code.min(k);
                i.end_code = i.end_code.max(kf);
            }
            i.start_data = i.start_data.max(k);
            i.end_data = i.end_data.max(kf);
            let start = k & !(PAGE - 1);
            let end = k.wrapping_add(l.memsz).next_multiple_of(PAGE);
            let file_end = if l.filesz == 0 { start } else { kf.next_multiple_of(PAGE).min(end) };
            if i.segs.try_reserve(1).is_ok() {
                i.segs.push(ImageSeg { start, file_end, end, off: l.offset & !(PAGE - 1) });
            }
        }
        if i.start_code == u64::MAX {
            i.start_code = 0;
        }
        i.segs.sort_by_key(|s| s.start);
        i.start_stack = img.rsp;
        i.arg_start = img.arg_start;
        i.arg_end = img.env_start;
        i.env_start = img.env_start;
        i.env_end = img.execfn;
        i
    }
}

/// The signal sets of one thread as `task_sig` collects them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SigView {
    /// The thread's own pending set (`p->pending`).
    pub pending: u64,
    /// The group's shared pending set.
    pub shared: u64,
    pub blocked: u64,
    /// SIG_IGN handlers (`collect_sigign_sigcatch`).
    pub ignored: u64,
    /// Handlers that are neither SIG_DFL nor SIG_IGN.
    pub caught: u64,
    /// Queued siginfos (SigQ's first number).
    pub queued: u64,
    /// Parked in a group stop (state T).
    pub stopped: bool,
}

impl Group {
    /// AB-7c getrusage RUSAGE_SELF/THREAD `ru_maxrss` in pages: the earlier images' peak and the current
    /// image's (`mm`) high-water mark (linux-6.6/kernel/sys.c:1831,1847 `setmax_mm_hiwater_rss`).
    pub fn maxrss_pages(&self, mm: &mm::AddressSpace) -> u64 {
        self.maxrss.load(Ordering::Relaxed).max(mm.hiwater_rss_pages())
    }
    /// RUSAGE_CHILDREN's `ru_maxrss` in pages (`signal->cmaxrss`).
    pub fn children_maxrss_pages(&self) -> u64 {
        self.cmaxrss.load(Ordering::Relaxed)
    }
    /// `getrusage(p, RUSAGE_BOTH)`'s `ru_maxrss` in pages, what wait4/waitid report for a collected child:
    /// the larger of its own peak and its children's (linux-6.6/kernel/sys.c:1803-1832, kernel/exit.c:1192).
    pub fn both_maxrss_pages(&self, mm: &mm::AddressSpace) -> u64 {
        self.maxrss_pages(mm).max(self.children_maxrss_pages())
    }
    /// An image's memory is left (execve, fs/exec.c:1035 `exec_mmap`): its peak stays the group's.
    pub(crate) fn note_maxrss(&self, mm: &mm::AddressSpace) {
        self.maxrss.fetch_max(mm.hiwater_rss_pages(), Ordering::Relaxed);
    }
    /// The parent collected `child` (kernel/exit.c:1182 `wait_task_zombie`): the larger of the child's own
    /// peak and its children's raises the parent's `cmaxrss`.
    pub(crate) fn note_child_maxrss(&self, child: &crate::Process) {
        self.cmaxrss.fetch_max(child.both_maxrss_pages(&child.mm), Ordering::Relaxed);
    }
    /// `task_sig` for the thread with signal key `key` (None: the group's sets with an empty thread part).
    pub fn sig_view(&self, key: Option<u64>) -> SigView {
        self.signals.raw(|s| {
            let mut v = SigView { shared: s.shared.set, queued: s.queued_count(), ..Default::default() };
            if let Some(t) = key.and_then(|k| s.thread(k)) {
                v.pending = t.pending.set;
                v.blocked = t.blocked;
                v.stopped = t.stopped;
            }
            for sig in 1..=64u32 {
                match s.action(sig).handler {
                    SIG_DFL => {}
                    SIG_IGN => v.ignored |= 1 << (sig - 1),
                    _ => v.caught |= 1 << (sig - 1),
                }
            }
            v
        })
    }

    /// `/proc/<pid>/oom_score_adj` (Linux `signal->oom_score_adj`).
    pub fn oom_score_adj(&self) -> i32 {
        oom_unpack(self.oom.load(Ordering::Acquire)).0
    }

    /// `__set_oom_adj` (non-legacy): EINVAL outside -1000..=1000 (checked by the caller as Linux's write does),
    /// EACCES below `oom_score_adj_min` without CAP_SYS_RESOURCE; a privileged writer also sets the minimum.
    /// Check and store are one compare-and-swap of the packed pair, so two writers never mix their halves.
    pub fn set_oom_score_adj(&self, v: i32, privileged: bool) -> Result<(), Errno> {
        if !(-1000..=1000).contains(&v) {
            return Err(EINVAL);
        }
        let mut cur = self.oom.load(Ordering::Acquire);
        loop {
            let (_, min) = oom_unpack(cur);
            if v < min && !privileged {
                return Err(EACCES);
            }
            let new = oom_pack(v, if privileged { v } else { min });
            match self.oom.compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(now) => cur = now,
            }
        }
        klog::event!(Info, "proc", "oom_score_adj", pid = self.tgid, value = v);
        Ok(())
    }

    /// fork (`copy_signal`): the child starts with the parent's value and minimum.
    pub(crate) fn copy_oom_from(&self, parent: &Group) {
        self.oom.store(parent.oom.load(Ordering::Acquire), Ordering::Release);
    }
}

/// (value, minimum) in one word: value in the low half, minimum in the high half.
fn oom_pack(v: i32, min: i32) -> u64 {
    (v as u32 as u64) | ((min as u32 as u64) << 32)
}

fn oom_unpack(w: u64) -> (i32, i32) {
    (w as u32 as i32, (w >> 32) as u32 as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elf::Load;

    fn elf(loads: Vec<Load>) -> Elf {
        Elf { entry: 0, is_pie: false, phdr_vaddr: 0, phoff: 0, phnum: 0, loads, interp: None, exec_stack: false, max_align: PAGE }
    }

    #[test]
    fn oom_pair_round_trips() {
        for (v, m) in [(0, 0), (-1000, 1000), (1000, -1000), (-1, -300), (500, 100)] {
            assert_eq!(oom_unpack(oom_pack(v, m)), (v, m));
        }
    }

    #[test]
    fn layout_follows_load_elf_binary() {
        let e = elf(alloc::vec![
            Load { vaddr: 0x400000, offset: 0, filesz: 0x1000, memsz: 0x1000, flags: 4, align: PAGE },
            Load { vaddr: 0x401000, offset: 0x1000, filesz: 0x2345, memsz: 0x2345, flags: 5, align: PAGE },
            Load { vaddr: 0x404e10, offset: 0x3e10, filesz: 0x100, memsz: 0x3000, flags: 6, align: PAGE },
        ]);
        let img = stack::Image { rsp: 0x7ff0, bytes: Vec::new(), arg_start: 0x7f00, env_start: 0x7f40, execfn: 0x7f80, platform: 0, random: 0, auxv: Vec::new() };
        let i = ImageInfo::of_load(&e, 0, &img);
        assert_eq!((i.start_code, i.end_code), (0x401000, 0x403345));
        assert_eq!((i.start_data, i.end_data), (0x404e10, 0x404f10));
        assert_eq!((i.arg_start, i.arg_end, i.env_start, i.env_end, i.start_stack), (0x7f00, 0x7f40, 0x7f40, 0x7f80, 0x7ff0));
        assert_eq!(i.segs[2], ImageSeg { start: 0x404000, file_end: 0x405000, end: 0x408000, off: 0x3000 });
        assert_eq!(i.segs[1], ImageSeg { start: 0x401000, file_end: 0x404000, end: 0x404000, off: 0x1000 });
    }
}
