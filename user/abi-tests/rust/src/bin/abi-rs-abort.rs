//! std::process::abort (musl's abort(): SIGABRT with all signals blocked, then the default action): the
//! program ends by SIGABRT on both kernels.
fn main() {
    println!("aborting");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    std::process::abort();
}
