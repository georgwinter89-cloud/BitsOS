//! Lines into the kernel log (`/dev/kmsg`, opened once before the seccomp filter closes the file system), else
//! stderr. The kernel's events of the gate are the journal's record of each call; these lines say what the
//! service itself did (start, refusals of tools it does not offer, errors).

use std::io::Write;
use std::sync::{Mutex, OnceLock};

static KMSG: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

/// Opens /dev/kmsg for later lines (false: the lines go to stderr).
pub fn open() -> bool {
    match std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        Ok(f) => KMSG.set(Mutex::new(f)).is_ok(),
        Err(_) => false,
    }
}

fn emit(prio: &str, msg: &str) {
    let line = format!("bitsmcp: {msg}\n");
    let sent = KMSG.get().is_some_and(|m| m.lock().is_ok_and(|mut f| f.write_all(format!("{prio}{line}").as_bytes()).is_ok()));
    if !sent {
        eprint!("{line}");
    }
}

pub fn info(msg: &str) {
    emit("<6>", msg);
}

pub fn warn(msg: &str) {
    emit("<4>", msg);
}
