//! assist-fake-serve: the service of protocol v2 (plan/spec/assistent-eins K1) with a scripted model, for the
//! views' tests and for trying a view by hand (see `assist::serve::fake` for the script).
//!
//!     cargo run -p assist --example fake_serve -- --listen /tmp/wy-a.sock --script skript.json [--chats DIR]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let after = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let (Some(listen), Some(script)) = (after("--listen"), after("--script")) else {
        eprintln!("assist-fake-serve --listen SOCKET --script FILE [--chats DIR]");
        std::process::exit(2);
    };
    let script = match std::fs::read_to_string(&script).map_err(|e| e.to_string()).and_then(|t| assist::serve::fake::Script::parse(&t)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("assist-fake-serve: {script}: {e}");
            std::process::exit(2);
        }
    };
    let chats = after("--chats").map_or_else(|| std::env::temp_dir().join(format!("assist-fake-chats-{}", std::process::id())), std::path::PathBuf::from);
    if let Err(e) = assist::serve::fake::start(std::path::Path::new(&listen), chats, script) {
        eprintln!("assist-fake-serve: {e}");
        std::process::exit(2);
    }
    println!("assist-fake-serve: listening on {listen}");
    loop {
        std::thread::park();
    }
}
