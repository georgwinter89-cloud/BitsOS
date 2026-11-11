//! `bitsos-install`: puts a BitsOS release onto a USB stick, or into an image for QEMU (cli.rs has the options, flow.rs the run).

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match bitsos_install::cli::Opts::parse(args) {
        Ok(None) => {
            let qemu = std::env::args().any(|a| a == "--qemu");
            println!("{}", if qemu { bitsos_install::cli::QEMU_HELP } else { bitsos_install::cli::HELP });
            return;
        }
        Ok(Some(o)) if o.qemu.is_some() => bitsos_install::qemu::run(&o),
        Ok(Some(o)) => bitsos_install::flow::run(o),
        Err(e) => Err(e),
    };
    if let Err(e) = r {
        eprintln!("bitsos-install: {e}");
        std::process::exit(1);
    }
}
