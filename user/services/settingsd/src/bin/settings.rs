//! `settings <command>`: the client of settingsd (see `settingsd::client`).
#![forbid(unsafe_code)]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(settingsd::client::main(&args));
}
