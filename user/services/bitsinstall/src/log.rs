//! Lines into the kernel log (`/dev/kmsg`), which become events that bitsjournal keeps: every decision, every
//! grant change, the confirmer coming and going. A kmsg line is cut at about 240 bytes: lines stay short and
//! carry no argument values (secrets stay out of the journal, R8i). Without /dev/kmsg: stderr.

use std::io::Write;
use std::sync::{Mutex, OnceLock};

static KMSG: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

pub fn open() -> bool {
    match std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        Ok(f) => KMSG.set(Mutex::new(f)).is_ok(),
        Err(_) => false,
    }
}

fn emit(prio: &str, msg: &str) {
    let line = format!("bitsinstall: {msg}\n");
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
