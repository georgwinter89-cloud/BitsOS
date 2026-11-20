//! A line typed without echo (`bitsupdate token`): the console's text grid and scrollback must never hold the
//! token, since MCP's `screen` tool (format=text) hands that grid out. As passwd and ssh do: ECHO off while the
//! line is read (linux-7.3.1/drivers/tty/n_tty.c `n_tty_receive_char`: no ECHO, nothing echoed), restored after.

use std::io::BufRead;

const TCGETS: u32 = 0x5401;
const TCSETS: u32 = 0x5402;
/// asm-generic/termbits.h: `c_lflag` is the fourth 32-bit word, ECHO its bit 0o10.
const LFLAG: usize = 12;
const ECHO: u32 = 0o10;

/// The terminal's settings as they were; put back on drop.
struct Restore {
    fd: usize,
    t: [u8; 36],
}

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = usys::dev::ioctl_buf(self.fd, TCSETS, &mut self.t);
    }
}

fn no_echo(fd: usize) -> Option<Restore> {
    let mut t = [0u8; 36];
    usys::dev::ioctl_buf(fd, TCGETS, &mut t).ok()?;
    let old = Restore { fd, t };
    let lflag = u32::from_ne_bytes(t[LFLAG..LFLAG + 4].try_into().ok()?);
    t[LFLAG..LFLAG + 4].copy_from_slice(&(lflag & !ECHO).to_ne_bytes());
    usys::dev::ioctl_buf(fd, TCSETS, &mut t).ok()?;
    Some(old)
}

/// `prompt`, then one line from stdin, echo off while it is typed when stdin is a terminal (not one: a pipe,
/// nothing to hide). Echo goes off before the prompt shows, so nothing typed after it can be echoed. `Err` when
/// stdin is a terminal whose echo could not be turned off: then nothing is read.
pub fn read_secret_line(prompt: &str) -> Result<String, String> {
    let restore = no_echo(0);
    if restore.is_none() && usys::dev::ioctl_buf(0, TCGETS, &mut [0u8; 36]).is_ok() {
        return Err(String::from("Die Konsole lässt das Verbergen nicht zu; nichts gelesen."));
    }
    println!("{prompt}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let mut line = String::new();
    let r = std::io::stdin().lock().read_line(&mut line);
    drop(restore);
    r.map_err(|e| format!("stdin: {e}"))?;
    Ok(line)
}
