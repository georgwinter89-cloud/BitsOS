//! xtask: the host tool of the desk. `cargo xtask <cmd>` (alias in .cargo/config.toml).
mod abi_diff;
mod cpio;
mod deps;
mod dropcheck;
mod image;
mod ktest;
mod qemu;
mod refuse;
mod serve;
mod stick;
mod suite;
mod util;

/// Expected `rustc -V` date of the pinned nightly. The desk bumps this (and rust-toolchain.toml's note) once a month.
pub const EXPECTED_RUSTC_DATE: &str = "2026-10-13";

const USAGE: &str = "usage: cargo xtask <command>
  build [--release] [--ktest]
  initrd [--out F]
  image [--name N] [--gen G] [--config-extra \"k=v;k2=v2\"] [--release] [--ktest]
  qemu [--name N] [--smp N] [--mem M] [--net none|user|igb|virtio] [--hostfwd H:G]... [--res WxH]
       [--display none|gtk] [--timeout S] [--serial-log F] [-- extra qemu args]
  ktest [FILTER] [--split f1,f2,...] [--timeout S] [--config-extra \"k=v;k2=v2\"] [--release]
  test [--no-ktest]
  fullrun [--quick] [--script-timeout S]
  stick --target <mountpoint> [--write] [--retire-v1 | --bundle D | --build | --confirm N | --prune ...] (cargo xtask stick --help)
  serve --dir D --port P
  deps
  toolchain";

fn main() {
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv[0] == "-h" || argv[0] == "--help" || argv[0] == "help" {
        println!("{USAGE}");
        return;
    }
    let cmd = argv.remove(0);
    let root = util::root();
    let a = util::Args::new(argv);
    let r = match cmd.as_str() {
        "build" => image::cmd_build(&root, a),
        "initrd" => image::cmd_initrd(&root, a),
        "image" => image::cmd_image(&root, a),
        "qemu" => qemu::cmd_qemu(&root, a),
        "ktest" => ktest::cmd_ktest(&root, a),
        "test" => suite::cmd_test(&root, a),
        "fullrun" => suite::cmd_fullrun(&root, a),
        "stick" => stick::cmd_stick(&root, a),
        "serve" => serve::cmd_serve(&root, a),
        "deps" => deps::cmd_deps(&root, a),
        "toolchain" => suite::cmd_toolchain(&root, a),
        "abi-diff" => abi_diff::cmd_abi_diff(&root, a),
        other => Err(format!("unknown command '{other}'\n{USAGE}")),
    };
    if let Err(e) = r {
        eprintln!("xtask: {e}");
        std::process::exit(1);
    }
}
