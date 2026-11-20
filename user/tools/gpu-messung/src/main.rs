//! gpu-messung: the measurement run of the Gaming-PC's RX 9070 XT, asked for at busybox's shell (`/ #`). On the
//! Gaming-PC the shell holds the console, so the kernel monitor's command (`bits>`) is out of reach; this program
//! asks the kernel through `/dev/bitsos/gpu-messung` and the kernel does the rest exactly as from `bits>`: the
//! explanation, the J question, the run, the protocol, the power-off (kernel/src/gpmessung.rs).
//!
//! It decides nothing itself: only a J of a real keyboard starts the card work (the kernel takes and ignores
//! keys a program or MCP injected), and the kernel refuses the node to a program MCP's run started. While the
//! question and the run stand, the screen is the kernel's; this program prints only its one line at the end.
//! Exit codes: 0 the run went (the PC did not switch off), 1 nothing was done, 2 usage.
#![forbid(unsafe_code)]

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::process::exit;

const NODE: &str = "/dev/bitsos/gpu-messung";

/// Linux errno values the kernel answers at the open.
const EPERM: i32 = 1;
const ENOENT: i32 = 2;
const EACCES: i32 = 13;

fn fail(line: &str) -> ! {
    eprintln!("{line}");
    exit(1)
}

/// The kernel's answer: "<exit code> <German line>".
fn parse(answer: &str) -> (i32, &str) {
    let a = answer.trim_end();
    match a.split_once(' ') {
        Some((c, line)) if c.parse::<i32>().is_ok() => (c.parse().unwrap_or(1), line),
        _ => (1, a),
    }
}

fn main() {
    if std::env::args().len() > 1 {
        eprintln!("Aufruf: gpu-messung (ohne Zusatz). Startet die Messung der RX 9070 XT; sie fragt vorher nach der Taste J.");
        exit(2);
    }
    let mut f = match OpenOptions::new().read(true).write(true).open(NODE) {
        Ok(f) => f,
        Err(e) => match e.raw_os_error() {
            Some(EPERM) => fail("Über die Fernsteuerung (MCP) gestartete Programme dürfen die Messung nicht anstoßen: bitte gpu-messung am PC selbst tippen. Es wurde nichts getan."),
            Some(EACCES) => fail("Die Messung darf nur root anstoßen. Es wurde nichts getan."),
            Some(ENOENT) => fail("Dieser BitsOS-Kern kennt die Messung nicht (es fehlt /dev/bitsos/gpu-messung). Es wurde nichts getan."),
            _ => fail(&format!("{NODE} ließ sich nicht öffnen ({e}). Es wurde nichts getan.")),
        },
    };
    if let Err(e) = f.write_all(b"start\n") {
        fail(&format!("Die Messung ließ sich nicht anstoßen ({e}). Es wurde nichts getan."));
    }
    // Waits until the kernel says how the start ended: declined, no key, no card here, or the run's end.
    let mut answer = String::new();
    if let Err(e) = f.read_to_string(&mut answer) {
        fail(&format!("Die Antwort des Kerns ließ sich nicht lesen ({e})."));
    }
    let (code, line) = parse(&answer);
    println!("{line}");
    exit(code)
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn answers() {
        assert_eq!(parse("1 Abgebrochen: es wurde nichts getan.\n"), (1, "Abgebrochen: es wurde nichts getan."));
        assert_eq!(parse("0 Die Messung ist zu Ende.\n"), (0, "Die Messung ist zu Ende."));
        assert_eq!(parse("kaputt"), (1, "kaputt"));
        assert_eq!(parse(""), (1, ""));
    }
}
