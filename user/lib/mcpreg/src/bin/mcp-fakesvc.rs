//! mcp-fakesvc: a test service for MC-6 (ktests `mc.svc.user_*`, Mini-PC checks). It registers tools named
//! after it through mcpreg and behaves by mode:
//!   echo  <name>        <name>_echo (read: "echo <arguments>", one progress line first), <name>_wipe
//!                       (delete_overwrite: answers "wiped nothing")
//!   hang  <name>        <name>_hang (read): never answers; prints "cancelled <id> <reason>" per cancel
//!   crash <name>        <name>_crash (change): aborts the process (SIGABRT) on the first call
//!   many  <name> <n>    <name>_t000 .. (read, echo)
//!   bad   <name>        registers a tool with a bad schema, prints the refusal, exits 3
//!   fork  <name>        <name>_fork (read), then forks: the child prints "child <pid>" and sleeps 60 s holding
//!                       the socket, the parent exits 0 (its tools must vanish though no EOF comes)
//! Socket: BITSOS_MCP_SOCK or /run/bitsos/mcp.sock. Prints "registered <n>" once its tools are there.

use mcpreg::{Event, Reply, Safety, Service, Tool};
use std::io::Write;

const SCHEMA: &str = r#"{"type":"object","properties":{"n":{"type":"integer"},"text":{"type":"string"}}}"#;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("echo");
    let name = args.get(2).cloned().unwrap_or_else(|| "fakesvc".to_string());
    let count: usize = args.get(3).and_then(|n| n.parse().ok()).unwrap_or(100);
    if let Err(e) = run(mode, &name, count) {
        eprintln!("mcp-fakesvc: {e}");
        std::process::exit(if matches!(e, mcpreg::Error::Refused(_)) { 3 } else { 1 });
    }
}

fn tool(name: String, safety: Safety, schema: &str) -> Result<Tool, mcpreg::Error> {
    Tool::new(&name, &format!("mcp-fakesvc test tool {name}"), schema, safety)
}

fn run(mode: &str, name: &str, count: usize) -> Result<(), mcpreg::Error> {
    let mut s = Service::connect(name)?;
    let tools = match mode {
        "echo" => vec![tool(format!("{name}_echo"), Safety::Read, SCHEMA)?, tool(format!("{name}_wipe"), Safety::DeleteOverwrite, SCHEMA)?],
        "hang" => vec![tool(format!("{name}_hang"), Safety::Read, SCHEMA)?],
        "crash" => vec![tool(format!("{name}_crash"), Safety::Change, SCHEMA)?],
        "many" => (0..count).map(|i| tool(format!("{name}_t{i:03}"), Safety::Read, SCHEMA)).collect::<Result<_, _>>()?,
        "bad" => vec![tool(format!("{name}_bad"), Safety::Read, r#"{"type":"string"}"#)?],
        "fork" => vec![tool(format!("{name}_fork"), Safety::Read, SCHEMA)?],
        other => return Err(mcpreg::Error::Protocol(format!("unknown mode {other} (echo, hang, crash, many, bad, fork)"))),
    };
    let n = s.register(&tools)?.len();
    println!("registered {n}");
    let _ = std::io::stdout().flush();
    if mode == "fork" {
        // Never returns in the child; the parent leaves at once (the child keeps `s` open).
        fork_and_hold();
        std::process::exit(0);
    }
    let rep = s.replier();
    while let Some(ev) = s.next()? {
        match ev {
            Event::Cancelled { id, reason } => {
                println!("cancelled {id} {reason}");
                let _ = std::io::stdout().flush();
            }
            Event::Call(c) => match mode {
                "hang" => {}
                "crash" => std::process::abort(),
                _ if c.name.ends_with("_wipe") => rep.reply(c.id, Reply::text("wiped nothing"))?,
                _ => {
                    rep.progress(c.id, "working")?;
                    rep.reply(c.id, Reply::text(format!("echo {}", c.arguments)))?;
                }
            },
        }
    }
    Ok(())
}

/// fork(2) without libc (the user workspace has none): the child sleeps holding every fd, then exits.
fn fork_and_hold() {
    let r: i64;
    // SAFETY: fork takes no arguments and touches no memory of ours; this program is single-threaded here,
    // so the child may go on using std (no lock can be held by a thread that is gone).
    unsafe {
        std::arch::asm!("syscall", inlateout("rax") 57i64 => r, lateout("rcx") _, lateout("r11") _, options(nostack));
    }
    if r == 0 {
        println!("child {}", std::process::id());
        let _ = std::io::stdout().flush();
        std::thread::sleep(std::time::Duration::from_secs(60));
        std::process::exit(0);
    }
    if r < 0 {
        eprintln!("mcp-fakesvc: fork: errno {}", -r);
        std::process::exit(1);
    }
}
