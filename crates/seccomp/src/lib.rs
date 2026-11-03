//! seccomp-bpf (owner: SX; SX-1, linux-6.6/kernel/seccomp.c as the reference for behaviour).
//!
//! - `bpf`: the classic-BPF verifier (Linux's rules for seccomp programs) and the interpreter.
//! - `filter`: installed filters, their stack, the action precedence and the action cache.
//! - `task`: a thread's mode and stack, strict mode, attach, TSYNC, inheritance, the per-call check.
//!
//! The syscall side (seccomp(2), prctl's PR_SET/GET_SECCOMP, the dispatcher's hook, SIGSYS, /proc status) is
//! glue in `crates/syscall` and `crates/proc` (granted to SX for SX-1).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod bpf;
pub mod consts;
pub mod filter;
pub mod task;

pub use bpf::{Insn, Program, Refused};
pub use filter::{Data, Filter};
pub use task::{AttachError, ChangeGuard, TaskSeccomp, Verdict, action_available, change};

use alloc::vec::Vec;

/// A user program's bytes (`len` sock_filter records) checked into a program.
pub fn load(bytes: &[u8]) -> Result<Program, Refused> {
    if bytes.is_empty() || bytes.len() % consts::INSN_SIZE != 0 {
        return Err(Refused::Length);
    }
    let mut insns = Vec::new();
    insns.try_reserve_exact(bytes.len() / consts::INSN_SIZE).map_err(|_| Refused::Length)?;
    for c in bytes.chunks_exact(consts::INSN_SIZE) {
        let mut b = [0u8; 8];
        b.copy_from_slice(c);
        insns.push(Insn::from_bytes(&b));
    }
    Program::load(&insns)
}

#[cfg(test)]
mod tests;
