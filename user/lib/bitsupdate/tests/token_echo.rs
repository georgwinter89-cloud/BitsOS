//! `bitsupdate token` on a terminal: the typed token never comes back as echo (the console's text grid, which
//! MCP's `screen` tool hands out, must not hold it). A pseudo terminal stands in for the console.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const TIOCGPTN: u32 = 0x8004_5430;
const TIOCSPTLCK: u32 = 0x4004_5431;

#[test]
fn the_token_is_not_echoed() {
    let master = std::fs::OpenOptions::new().read(true).write(true).open("/dev/ptmx").expect("/dev/ptmx");
    let fd = master.as_raw_fd() as usize;
    usys::dev::ioctl_buf(fd, TIOCSPTLCK, &mut [0u8; 4]).expect("unlock");
    let mut n = [0u8; 4];
    usys::dev::ioctl_buf(fd, TIOCGPTN, &mut n).expect("pty number");
    let slave_path = format!("/dev/pts/{}", u32::from_ne_bytes(n));
    let slave = std::fs::OpenOptions::new().read(true).write(true).open(&slave_path).expect("slave");
    let dir = std::env::temp_dir().join(format!("bitsupdate-echo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut child = Command::new(env!("CARGO_BIN_EXE_bitsupdate"))
        .arg("token")
        .env("BITSUPDATE_DIR", &dir)
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave))
        .spawn()
        .expect("bitsupdate");
    // The reader runs until the child closed the terminal's last other end (EIO).
    let mut r = master.try_clone().unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let reader = std::thread::spawn(move || {
        let mut b = [0u8; 256];
        while let Ok(k) = r.read(&mut b) {
            if k == 0 {
                break;
            }
            s2.lock().unwrap().extend_from_slice(&b[..k]);
        }
    });
    let t0 = Instant::now();
    while !String::from_utf8_lossy(&seen.lock().unwrap()).contains("Enter drücken") {
        assert!(t0.elapsed() < Duration::from_secs(20), "no prompt");
        std::thread::sleep(Duration::from_millis(20));
    }
    let token = "test_echo_SECRETXYZ0123456789";
    (&master).write_all(format!("{token}\n").as_bytes()).unwrap();
    assert!(child.wait().unwrap().success());
    reader.join().unwrap();
    let all = String::from_utf8_lossy(&seen.lock().unwrap()).to_string();
    assert!(all.contains("Gespeichert."), "{all}");
    assert!(all.contains(&format!("*** ({} Zeichen)", token.len())), "{all}");
    assert!(!all.contains("SECRETXYZ"), "the token was echoed: {all}");
    assert_eq!(std::fs::read_to_string(dir.join(bitsupdate::source::TOKEN)).unwrap().trim(), token);
    let _ = std::fs::remove_dir_all(&dir);
}
