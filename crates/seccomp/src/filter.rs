//! Filters and their stack (linux-6.6/kernel/seccomp.c `struct seccomp_filter`, `seccomp_run_filters`,
//! `seccomp_cache_prepare`): every filter points at the one installed before it; a thread holds the newest.
//! Filters are shared (fork, clone and TSYNC hand the same stack on) and never change once installed.

use crate::bpf::Program;
use crate::consts::*;
use alloc::sync::Arc;

/// System call numbers the action cache covers (x86-64's table ends at 461 in 6.6; numbers past the window,
/// x32 numbers included, simply run the filters). An optimisation window, not a count of things (R1).
pub const CACHE_NRS: usize = 512;
pub const CACHE_WORDS: usize = CACHE_NRS / 64;

/// The 64 bytes a filter reads (`struct seccomp_data`), as 16 little-endian words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Data {
    pub nr: u32,
    pub arch: u32,
    pub ip: u64,
    pub args: [u64; 6],
}

impl Data {
    pub fn words(&self) -> [u32; 16] {
        let mut w = [0u32; 16];
        w[0] = self.nr;
        w[1] = self.arch;
        w[2] = self.ip as u32;
        w[3] = (self.ip >> 32) as u32;
        for (i, a) in self.args.iter().enumerate() {
            w[4 + 2 * i] = *a as u32;
            w[5 + 2 * i] = (*a >> 32) as u32;
        }
        w
    }
}

/// One installed filter.
#[derive(Debug)]
pub struct Filter {
    prog: Program,
    /// SECCOMP_FILTER_FLAG_LOG: its non-allow answers are logged.
    log: bool,
    prev: Option<Arc<Filter>>,
    /// Filters in the stack up to this one (/proc's `Seccomp_filters`).
    depth: u32,
    /// Runtime instructions of the stack up to this one with Linux's 4 per filter on top (MAX_INSNS_PER_PATH;
    /// `Program::runtime_len`).
    path: usize,
    /// Calls this stack allows whatever their arguments (`seccomp_cache_check_allow`).
    allow: [u64; CACHE_WORDS],
}

impl Filter {
    /// `seccomp_attach_filter`'s length rule: would `prog` on top of `prev` stay within MAX_INSNS_PER_PATH?
    pub fn fits(prog: &Program, prev: Option<&Filter>) -> bool {
        prog.runtime_len() + prev.map_or(0, |p| p.path) <= MAX_INSNS_PER_PATH
    }
    /// A filter on top of `prev` (the caller checked `fits`).
    pub fn new(prog: Program, log: bool, prev: Option<Arc<Filter>>) -> Filter {
        let mut allow = match &prev {
            Some(p) => p.allow,
            None => [!0u64; CACHE_WORDS],
        };
        for (w, word) in allow.iter_mut().enumerate() {
            let mut bits = *word;
            while bits != 0 {
                let b = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                if !prog.const_allow((w * 64 + b) as u32, AUDIT_ARCH_X86_64) {
                    *word &= !(1u64 << b);
                }
            }
        }
        let depth = prev.as_ref().map_or(0, |p| p.depth) + 1;
        let path = prev.as_ref().map_or(0, |p| p.path) + prog.runtime_len() + 4;
        Filter { prog, log, prev, depth, path, allow }
    }
    pub fn depth(&self) -> u32 {
        self.depth
    }
    pub fn prev(&self) -> Option<&Arc<Filter>> {
        self.prev.as_ref()
    }
    pub fn len(&self) -> usize {
        self.prog.len()
    }
    pub fn is_empty(&self) -> bool {
        self.prog.is_empty()
    }
    /// Whether `other` is this filter or one below it (`is_ancestor(other, self)`).
    pub fn has(&self, other: &Filter) -> bool {
        let mut f = Some(self);
        while let Some(x) = f {
            if core::ptr::eq(x, other) {
                return true;
            }
            f = x.prev.as_deref();
        }
        false
    }
    /// The action cache's words (bit n: call n is allowed whatever its arguments).
    pub fn allow_words(&self) -> &[u64; CACHE_WORDS] {
        &self.allow
    }
    /// Whether the action cache allows `d` without running a filter.
    pub fn cached_allow(&self, d: &Data) -> bool {
        let nr = d.nr as usize;
        d.arch == AUDIT_ARCH_X86_64 && nr < CACHE_NRS && self.allow[nr / 64] & (1u64 << (nr % 64)) != 0
    }

    /// `seccomp_run_filters`: every filter runs, newest first; the lowest action (signed compare of the
    /// action bits) wins, the newest among equals. Returns the value and whether the deciding filter logs.
    pub fn run(&self, d: &Data) -> (u32, bool) {
        if self.cached_allow(d) {
            return (RET_ALLOW, false);
        }
        let words = d.words();
        let mut ret = RET_ALLOW;
        let mut log = false;
        let mut f = Some(self);
        while let Some(x) = f {
            let r = x.prog.run(&words);
            if action_rank(r) < action_rank(ret) {
                ret = r;
                log = x.log;
            }
            f = x.prev.as_deref();
        }
        (ret, log)
    }
}

/// `ACTION_ONLY`: the action bits as a signed number (KILL_PROCESS, 0x80000000, ranks lowest).
pub fn action_rank(r: u32) -> i32 {
    (r & RET_ACTION_FULL) as i32
}

impl Drop for Filter {
    /// A stack can be thousands of filters deep: free it in a loop, not by recursion on the kernel stack.
    fn drop(&mut self) {
        let mut next = self.prev.take();
        while let Some(a) = next {
            match Arc::try_unwrap(a) {
                Ok(mut f) => next = f.prev.take(),
                Err(_) => break,
            }
        }
    }
}
