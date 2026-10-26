//! `settingsd [serve] [--stick DIR] [--fallback DIR] [--sys DIR] [--socket PATH] [--deadline-ms N]
//! [--mcp-socket PATH | --no-mcp]`: the service (init starts it, change request CL-1/2). The MCP socket defaults to
//! `BITSOS_MCP_SOCK` or `/run/bitsos/mcp.sock`. `settingsd <command>` is the `settings` client.
#![forbid(unsafe_code)]

use settingsd::events::Events;
use settingsd::registry::Registry;
use settingsd::store::Paths;
use settingsd::{client, mcp, server};
use std::sync::Arc;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| client::COMMANDS.contains(&a.as_str())) && args.first().map(String::as_str) != Some("serve")
    {
        std::process::exit(client::main(&args));
    }
    let mut paths = Paths::default();
    let mut deadline = Duration::from_secs(5);
    let mut mcp_socket = Some(std::env::var("BITSOS_MCP_SOCK").unwrap_or_else(|_| mcpreg::DEFAULT_SOCKET.into()));
    let mut it = args.iter().filter(|a| *a != "serve");
    while let Some(a) = it.next() {
        let mut v = || it.next().cloned().unwrap_or_default();
        match a.as_str() {
            "--stick" => paths.stick = v().into(),
            "--fallback" => paths.fallback = v().into(),
            "--sys" => paths.sys = v().into(),
            "--socket" => paths.socket = v().into(),
            "--mcp-socket" => mcp_socket = Some(v()),
            "--no-mcp" => mcp_socket = None,
            "--deadline-ms" => deadline = Duration::from_millis(v().parse().unwrap_or(5000)),
            other => {
                eprintln!("settingsd: unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }
    let socket = paths.socket.clone();
    let reg = Registry::open(paths, Events::system(), deadline);
    let listener = match server::bind(&socket, &reg) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("settingsd: {e}");
            std::process::exit(1);
        }
    };
    let shared = Arc::new(server::Shared::new(reg));
    if let Some(socket) = mcp_socket {
        let b = mcp::Bridge { socket: socket.into(), shared: shared.clone(), events: Events::system() };
        // Without the bridge settingsd still serves its socket; Claude then lacks only the tools.
        if let Err(e) = mcp::spawn(b) {
            eprintln!("settingsd: no MCP thread: {e}");
        }
    }
    server::serve(listener, shared);
}
