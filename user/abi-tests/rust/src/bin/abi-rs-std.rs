//! Rust std on V2 (AB-3 "Rust std hello does not abort"): start-up (std polls fds 0-2, ignores SIGPIPE,
//! installs SIGSEGV/SIGBUS handlers with a sigaltstack on a guarded mmap), args, env, stdin, files in /tmp,
//! Instant and SystemTime, HashMap's random keys (getrandom), available_parallelism (sched_getaffinity),
//! a caught panic (the panic hook writes to stderr), and an exit code.
use std::collections::HashMap;
use std::io::{IsTerminal, Read, Write};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    println!("args: {args:?}");
    println!("env ABI_TEST: {:?}", std::env::var("ABI_TEST"));
    println!("env NOPE: {:?}", std::env::var("NOPE").is_err());
    let mut input = String::new();
    let n = std::io::stdin().read_to_string(&mut input).unwrap_or(0);
    println!("stdin: {n} bytes {input:?}");
    println!("stdout is a terminal: {}", std::io::stdout().is_terminal());

    let t0 = Instant::now();
    std::thread::sleep(Duration::from_millis(20));
    println!("slept at least 20 ms: {}", t0.elapsed() >= Duration::from_millis(20));
    println!("SystemTime after the epoch: {}", SystemTime::now().duration_since(UNIX_EPOCH).is_ok());

    let mut m = HashMap::new();
    for i in 0..100 {
        m.insert(i, i * i);
    }
    println!("hashmap: {} entries, m[7] = {}", m.len(), m[&7]);
    println!("available_parallelism >= 1: {}", std::thread::available_parallelism().map(|n| n.get() >= 1).unwrap_or(false));

    let path = format!("/tmp/abi-rs-std-{}", std::process::id());
    let written = std::fs::write(&path, b"some bytes\n").is_ok();
    let back = std::fs::read_to_string(&path).unwrap_or_default();
    let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let mut f = std::fs::OpenOptions::new().append(true).open(&path).expect("open for append");
    let _ = f.write_all(b"more\n");
    drop(f);
    let after = std::fs::read_to_string(&path).unwrap_or_default();
    let removed = std::fs::remove_file(&path).is_ok();
    println!("file: written {written}, read {back:?}, len {len}, appended {after:?}, removed {removed}");
    println!("missing file: {:?}", std::fs::read("/tmp/abi-rs-std-does-not-exist").map_err(|e| e.kind()));
    println!("/tmp is a directory: {}", std::fs::metadata("/tmp").map(|m| m.is_dir()).unwrap_or(false));

    // Own hook: std's default message names the thread id, which differs between any two runs.
    std::panic::set_hook(Box::new(|info| {
        let msg = info.payload().downcast_ref::<&str>().copied().unwrap_or("?");
        eprintln!("panic hook: {msg}");
    }));
    let r = std::panic::catch_unwind(|| panic!("a panic on purpose"));
    println!("caught the panic: {}", r.is_err());
    let _ = std::io::stdout().flush();
    std::process::exit(3);
}
