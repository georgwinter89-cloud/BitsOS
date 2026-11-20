//! Checks a firmware file on demand, read from a path; no test touches a real file and none enters the repository.
//! Takes the unpacked file: `zstd -dc /usr/lib/firmware/rtl_nic/rtl8126a-3.fw.zst > x.fw`, then
//! `cargo run -p rtl8126 --example fwcheck -- x.fw`. For the hash: `sha256sum x.fw` against `FIRMWARE_3.sha256`.
//! Header and validator only: the licence forbids reverse engineering, so the actions are not listed.

use rtl8126::chip::FIRMWARE_3;
use rtl8126::fwscript::Script;

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: fwcheck <unpacked firmware file>");
        std::process::exit(2);
    };
    let file = match std::fs::read(&path) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("{path}: {e}");
            std::process::exit(2);
        }
    };
    let script = match Script::parse(&file) {
        Ok(script) => script,
        Err(e) => {
            println!("refused: {e:?}");
            std::process::exit(1);
        }
    };
    let version = String::from_utf8_lossy(script.version());
    println!("bytes    {}", file.len());
    println!("version  {version}");
    println!("fw_start {}", script.start());
    println!("fw_len   {}", script.len());
    let same = file.len() == FIRMWARE_3.bytes
        && version == FIRMWARE_3.version
        && script.start() == FIRMWARE_3.start
        && script.len() == FIRMWARE_3.actions as usize;
    println!("same as {}: {}", FIRMWARE_3.name, if same { "yes" } else { "no" });
    println!("expected sha256 {}", FIRMWARE_3.sha256);
}
