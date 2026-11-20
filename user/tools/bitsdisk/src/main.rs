//! `bitsdisk`: gives BitsOS one partition of a disk (NV-2p). It runs at the console and only shows and asks; the
//! kernel (`/dev/bitsos/disksetup`) checks everything and reads the typed code from the keyboard itself. Over MCP the
//! device does not open. `nvme_write=on` stays Georg's own edit. `bitsdisk umzug` (GP-DISK) copies the stick's old
//! data to the new /data once; it asks for the same device first, so it too runs only as root at the machine.

#![forbid(unsafe_code)]

use bitsdisk::{Dialog, parse_list, refusal_text, render, setup, umzug};
use std::fs::OpenOptions;
use std::io::{self, BufRead, Read, Write};
use std::process::ExitCode;

const NODE: &str = "/dev/bitsos/disksetup";

/// One command, one answer (a fresh read after each write).
fn ask(dev: &mut std::fs::File, cmd: &str) -> io::Result<String> {
    dev.write_all(cmd.as_bytes())?;
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = dev.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn line() -> Option<String> {
    let mut s = String::new();
    match io::stdin().lock().read_line(&mut s) {
        Ok(n) if n > 0 => Some(s.trim().to_string()),
        _ => None,
    }
}

fn say(t: &str) {
    print!("{t}");
    let _ = io::stdout().flush();
}

/// The dialog at the machine: stdin, stdout and the kernel's device.
struct Console<'a>(&'a mut std::fs::File);

impl Dialog for Console<'_> {
    fn ask(&mut self, cmd: &str) -> io::Result<String> {
        ask(self.0, cmd)
    }
    fn line(&mut self) -> Option<String> {
        line()
    }
    fn say(&mut self, text: &str) {
        say(text);
    }
}

fn run() -> io::Result<bool> {
    let arg = std::env::args().nth(1);
    let mut dev = match OpenOptions::new().read(true).write(true).open(NODE) {
        Ok(d) => d,
        // EACCES: not root, which is what MCP's `run` is; EPERM: the kernel says no (MCP's program id).
        Err(e) if matches!(e.raw_os_error(), Some(1 | 13)) => {
            println!("bitsdisk geht nur als root an diesem Rechner, nie ueber MCP. Es wurde nichts geschrieben.");
            return Ok(false);
        }
        Err(e) => return Err(e),
    };
    if arg.as_deref() == Some("umzug") {
        return move_data();
    }
    let a = ask(&mut dev, "list\n")?;
    let Some(l) = parse_list(&a) else {
        println!("Die Antwort des Kerns ist nicht lesbar: {a}");
        return Ok(false);
    };
    match arg.as_deref() {
        Some("list") => {
            say(&render(&l));
            if !l.write_on {
                println!("\n{}", refusal_text("no_nvme_write", ""));
            }
            Ok(true)
        }
        None => setup(&mut Console(&mut dev), &l),
        Some(_) => {
            println!("bitsdisk [list|umzug]");
            Ok(false)
        }
    }
}

/// `bitsdisk umzug`: the stick's old data onto the SSD's /data, once (`bitsdisk::umzug`).
fn move_data() -> io::Result<bool> {
    let window = std::fs::read_to_string("/proc/bitsos/data-window").unwrap_or_default();
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let stick = mounts.lines().any(|l| l.split(' ').nth(1) == Some("/stickdata"));
    let marker = std::path::Path::new("/data").join(umzug::MARKER).exists();
    if let Some(why) = umzug::refuse(window.contains("window=armed"), stick, marker) {
        println!("{why}");
        return Ok(false);
    }
    println!("Die Daten vom Stick werden nach /data kopiert. Das kann ein paar Minuten dauern ...");
    let start = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let mut copy = || -> io::Result<bool> {
        let ok = std::process::Command::new("/bin/cp").args(["-a", "/stickdata/.", "/data/"]).status()?.success();
        Ok(ok && std::process::Command::new("/bin/sync").status()?.success())
    };
    let r = umzug::run(std::path::Path::new("/stickdata"), std::path::Path::new("/data"), start, &mut copy)?;
    say(&umzug::result_text(&r));
    Ok(r.is_ok())
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            println!("bitsdisk: {e}");
            ExitCode::FAILURE
        }
    }
}
