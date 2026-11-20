//! The desk's public release key into the binary: `keys/release.pub` (64 hex digits, `#` comment lines), or the
//! file `BITSOS_UPDATE_PUBKEY_FILE` names (the QEMU scenario's test key). `none`: no key yet.
use std::io::Write;

fn main() {
    println!("cargo:rerun-if-env-changed=BITSOS_UPDATE_PUBKEY_FILE");
    let path = std::env::var("BITSOS_UPDATE_PUBKEY_FILE").ok().filter(|p| !p.is_empty()).unwrap_or_else(|| String::from("keys/release.pub"));
    println!("cargo:rerun-if-changed={path}");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let line = text.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#')).unwrap_or("none");
    let key = if line == "none" {
        String::from("None")
    } else {
        if line.len() != 64 || !line.bytes().all(|b| b.is_ascii_hexdigit()) {
            panic!("{path}: the key is 64 hex digits (or the word none), got {line:?}");
        }
        let bytes: Vec<String> = (0..32).map(|i| format!("0x{}", &line[2 * i..2 * i + 2])).collect();
        format!("Some([{}])", bytes.join(", "))
    };
    let out = std::path::Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("key.rs");
    let mut f = std::fs::File::create(out).expect("key.rs");
    writeln!(f, "pub const DESK_KEY: Option<[u8; 32]> = {key};").unwrap();
}
