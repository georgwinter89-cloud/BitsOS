//! Questions at the terminal. A passphrase is read without echo where the terminal allows it (`stty` on Unix, the console mode on Windows).

use std::io::{self, BufRead, Write};

pub fn line(prompt: &str) -> Result<String, String> {
    print!("{prompt}");
    io::stdout().flush().map_err(|e| e.to_string())?;
    let mut l = String::new();
    let n = io::stdin().lock().read_line(&mut l).map_err(|e| e.to_string())?;
    if n == 0 {
        return Err("the input ended; nothing was written".into());
    }
    Ok(l.trim_end_matches(['\n', '\r']).to_string())
}

/// y/n with a default for a bare Enter.
pub fn yes(prompt: &str, default: bool) -> Result<bool, String> {
    loop {
        let a = line(&format!("{prompt} [{}] ", if default { "Y/n" } else { "y/N" }))?;
        match a.trim().to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" | "j" | "ja" => return Ok(true),
            "n" | "no" | "nein" => return Ok(false),
            _ => println!("Please answer y or n."),
        }
    }
}

#[cfg(unix)]
fn echo(on: bool) {
    let _ = std::process::Command::new("stty").arg(if on { "echo" } else { "-echo" }).stdin(std::process::Stdio::inherit()).status();
}
#[cfg(windows)]
fn echo(on: bool) {
    crate::devices::windows::user::echo(on);
}
#[cfg(not(any(unix, windows)))]
fn echo(_on: bool) {}

/// A secret: typed without echo; never printed back.
pub fn secret(prompt: &str) -> Result<String, String> {
    echo(false);
    let r = line(prompt);
    echo(true);
    println!();
    r
}
