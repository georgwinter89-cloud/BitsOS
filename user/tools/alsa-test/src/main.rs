//! alsa-test: a stub until SN-4 (item UL-4b). V2 has no sound devices yet (`/dev/snd` comes with SN-4's ALSA
//! PCM); the real test (open the PCM, play a tone, report the period timing) is written with SN-4.
//!
//! `alsa-test` prints that it is a stub and exits 77 ("skipped", the automake convention), so a check list or a
//! script can tell "not tested yet" from a pass (0) or a failure (1). `--help`: exit 0. It opens no device.
#![forbid(unsafe_code)]

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--help") | Some("-h") => {
            println!("usage: alsa-test\nA stub until SN-4 (ALSA PCM on V2): plays nothing, opens no device, exits 77 (skipped).");
        }
        Some(a) => {
            eprintln!("alsa-test: unknown argument {a}\nusage: alsa-test");
            std::process::exit(2);
        }
        None => {
            println!("alsa-test: stub until SN-4 (ALSA PCM on V2): nothing played, skipped");
            std::process::exit(77);
        }
    }
}
