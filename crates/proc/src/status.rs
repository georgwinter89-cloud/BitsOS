//! How a process ended (pure, host-tested): the exit status, its `wait4` encoding, the shell-style code, the
//! signal a CPU exception in user mode stands for, and the V2 fault message (R3, V1's YK-1 3.8 wording:
//! "Program <name> (pid <p>, thread <t>) ended: ERROR: <what> at <address>: <reason>").

use alloc::format;
use alloc::string::String;
use uapi::ab::signal::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitStatus {
    /// `exit`/`exit_group` with this status (the low 8 bits of the argument).
    Exited(u8),
    /// Ended by a signal (a fault, a kill).
    Signaled(u32),
}

impl ExitStatus {
    /// The `int` `wait4` stores: `status << 8` for an exit, the signal number for a kill (no core dumps, so
    /// bit 7 stays clear).
    pub fn wait_status(self) -> u32 {
        match self {
            ExitStatus::Exited(c) => (c as u32) << 8,
            ExitStatus::Signaled(s) => s & 0x7F,
        }
    }
    /// What a shell's `$?` shows: the status, or 128 + signal.
    pub fn shell_code(self) -> i32 {
        match self {
            ExitStatus::Exited(c) => c as i32,
            ExitStatus::Signaled(s) => 128 + s as i32,
        }
    }
    /// "exit 3" / "signal 11 (SIGSEGV)".
    pub fn describe(self) -> String {
        match self {
            ExitStatus::Exited(c) => format!("exit {c}"),
            ExitStatus::Signaled(s) => format!("signal {s} ({})", uapi::ab::signal_names::name(s)),
        }
    }
}

/// The signal Linux sends for a CPU exception taken in user mode (arch/x86/kernel/traps.c).
pub fn trap_signal(vector: u8) -> u32 {
    match vector {
        0 => SIGFPE,             // #DE divide error
        1 | 3 => SIGTRAP,        // #DB, #BP (int3)
        4 | 5 => SIGSEGV,        // #OF (into), #BR (bound)
        6 => SIGILL,             // #UD
        7 => SIGILL,             // #NM: never with eager FPU switching; Linux ends the task too
        10 => SIGSEGV,           // #TS (do_error_trap with SIGSEGV)
        11 | 12 => SIGBUS,       // #NP, #SS
        13 | 14 | 21 => SIGSEGV, // #GP, #PF, #CP
        16 | 19 => SIGFPE,       // #MF x87, #XM SIMD
        17 => SIGBUS,            // #AC alignment check
        _ => SIGSEGV,
    }
}

/// What happened, for the message: "Page fault", "General protection fault", ...
pub fn trap_what(vector: u8) -> &'static str {
    match vector {
        0 => "Divide error",
        1 => "Debug trap",
        3 => "Breakpoint",
        4 => "Overflow",
        5 => "Bound range exceeded",
        6 => "Invalid instruction",
        7 => "FPU not available",
        10 => "Invalid TSS",
        11 => "Segment not present",
        12 => "Stack fault",
        13 => "General protection fault",
        14 => "Page fault",
        16 => "x87 floating-point error",
        17 => "Alignment check",
        19 => "SIMD floating-point error",
        21 => "Control protection fault",
        _ => "CPU exception",
    }
}

/// Reason of a page fault from the error code alone (when the address space cannot say more).
pub fn page_fault_reason(error: u64) -> &'static str {
    if error & 1 == 0 {
        "not mapped"
    } else if error & 2 != 0 {
        "write to read-only memory"
    } else if error & 16 != 0 {
        "execute of non-executable memory"
    } else {
        "access not allowed"
    }
}

/// The V2 message (R3): program, pid, thread, what, address and reason, plus the instruction address.
/// A page fault names the faulting address; other exceptions name the instruction.
pub fn fault_message(program: &str, pid: u32, thread: u64, vector: u8, addr: u64, rip: u64, reason: &str) -> String {
    let what = trap_what(vector);
    if vector == 14 {
        format!("Program {program} (pid {pid}, thread {thread}) ended: ERROR: {what} at {addr:#x}: {reason} (rip {rip:#x})")
    } else {
        format!("Program {program} (pid {pid}, thread {thread}) ended: ERROR: {what} at rip {rip:#x}: {reason}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_status_like_linux() {
        // WIFEXITED(s) = (s & 0x7f) == 0, WEXITSTATUS(s) = (s >> 8) & 0xff, WTERMSIG(s) = s & 0x7f.
        let e = ExitStatus::Exited(42).wait_status();
        assert_eq!((e & 0x7f, (e >> 8) & 0xff), (0, 42));
        assert_eq!(ExitStatus::Signaled(SIGSEGV).wait_status(), 11);
        assert_eq!(ExitStatus::Signaled(SIGKILL).shell_code(), 137);
        assert_eq!(ExitStatus::Exited(255).shell_code(), 255);
        assert_eq!(ExitStatus::Signaled(11).describe(), "signal 11 (SIGSEGV)");
    }

    #[test]
    fn traps_map_to_linux_signals() {
        assert_eq!(trap_signal(14), SIGSEGV);
        assert_eq!(trap_signal(13), SIGSEGV);
        assert_eq!(trap_signal(10), SIGSEGV);
        assert_eq!(trap_signal(11), SIGBUS);
        assert_eq!(trap_signal(12), SIGBUS);
        assert_eq!(trap_signal(6), SIGILL);
        assert_eq!(trap_signal(0), SIGFPE);
        assert_eq!(trap_signal(3), SIGTRAP);
        assert_eq!(trap_signal(19), SIGFPE);
        assert_eq!(trap_signal(17), SIGBUS);
    }

    #[test]
    fn message_names_everything() {
        let m = fault_message("hello", 104, 9, 14, 0, 0x401234, "not mapped");
        assert_eq!(m, "Program hello (pid 104, thread 9) ended: ERROR: Page fault at 0x0: not mapped (rip 0x401234)");
        let m = fault_message("x", 1, 2, 6, 0, 0x5000, "invalid opcode");
        assert!(m.contains("Invalid instruction at rip 0x5000"));
        assert_eq!(page_fault_reason(0x4), "not mapped");
        assert_eq!(page_fault_reason(0x7), "write to read-only memory");
        assert_eq!(page_fault_reason(0x15), "execute of non-executable memory");
    }
}
