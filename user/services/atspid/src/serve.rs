//! `atspid serve`: binds C3 (as root when started by init, then gives the socket to the session's user and drops
//! to that user), waits for the switch, runs the accessibility bus (busd) and the registry on it, and answers C3.
//! Switched off while running, it ends; init starts it again and it waits.

use crate::atspi::{self, Bus};
use crate::c3::{self, State};
use crate::registry::{DeviceEvents, Registry, Socket};
use futures_util::future::{Either, select};
use futures_util::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener as StdListener, UnixStream as StdStream};
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;
use zruntime::net::unix::{UnixListener, UnixStream};
use zruntime::{Shared, SharedRuntime};

pub const BUS: &str = "/run/user/1000/at-spi/bus";
pub const SOCKET: &str = "/run/bitsos/atspi.sock";
pub const SESSION_UID: u32 = 1000;
/// How often it looks at the switch by default.
pub const LOOK_S: u64 = 30;

pub struct Opts {
    pub bus: PathBuf,
    pub socket: PathBuf,
    /// settingsd's socket; None: always on (tests).
    pub settings: Option<PathBuf>,
    pub uid: u32,
    /// How often the switch is looked at.
    pub look: Duration,
    /// Programs of the session's user that may ask besides the desktop (`--peer`, the host tests).
    pub peers: Vec<PathBuf>,
}

fn io(what: &str) -> impl Fn(std::io::Error) -> String + '_ {
    move |e| format!("{what}: {e}")
}

/// A socket file nobody answers on is left over from an earlier run; one that answers belongs to a running copy.
fn clear_stale(path: &Path) -> Result<(), String> {
    if path.symlink_metadata().is_err() {
        return Ok(());
    }
    if StdStream::connect(path).is_ok() {
        return Err(format!("{} answers already: another atspid runs", path.display()));
    }
    std::fs::remove_file(path).map_err(io("removing a stale socket"))
}

fn bind_c3(o: &Opts) -> Result<StdListener, String> {
    if let Some(d) = o.socket.parent() {
        std::fs::create_dir_all(d).map_err(io("socket folder"))?;
    }
    clear_stale(&o.socket)?;
    let l = StdListener::bind(&o.socket).map_err(io("binding the C3 socket"))?;
    std::fs::set_permissions(&o.socket, std::fs::Permissions::from_mode(0o600)).map_err(io("socket mode"))?;
    if rustix::process::geteuid().is_root() {
        std::os::unix::fs::chown(&o.socket, Some(o.uid), Some(o.uid)).map_err(io("socket owner"))?;
    }
    Ok(l)
}

/// Started as root (by init): from here on the session's user. Only one thread runs at this point, so the
/// per-thread calls change the whole process.
fn drop_root(uid: u32) -> Result<(), String> {
    use rustix::thread::{set_thread_groups, set_thread_res_gid, set_thread_res_uid};
    if !rustix::process::geteuid().is_root() {
        return Ok(());
    }
    let (u, g) = (rustix::process::Uid::from_raw(uid), rustix::process::Gid::from_raw(uid));
    set_thread_groups(&[]).map_err(|e| format!("setgroups: {e}"))?;
    set_thread_res_gid(g, g, g).map_err(|e| format!("setresgid: {e}"))?;
    set_thread_res_uid(u, u, u).map_err(|e| format!("setresuid: {e}"))?;
    if rustix::process::geteuid().is_root() {
        return Err("still root after setresuid".into());
    }
    Ok(())
}

pub fn run(o: Opts) -> Result<(), String> {
    let l = bind_c3(&o)?;
    drop_root(o.uid)?;
    let socket = o.socket.clone();
    let r = zruntime::block_on(serve(o, l));
    let _ = std::fs::remove_file(socket);
    r
}

async fn switched_on(settings: &Option<PathBuf>) -> bool {
    match settings.clone() {
        None => true,
        Some(p) => zruntime::unblock(move || crate::settings::enabled(&p)).await,
    }
}

async fn serve(o: Opts, l: StdListener) -> Result<(), String> {
    let rt = SharedRuntime::current().map_err(io("runtime"))?;
    let st = Arc::new(State::default());
    let listener = UnixListener::from_std(&rt, l).map_err(io("C3 socket"))?;
    let c3 = pin!(accept(st.clone(), listener, o.uid, o.peers.clone()));
    let main = pin!(async {
        while !switched_on(&o.settings).await {
            rt.sleep(o.look).await;
        }
        let r = run_bus(&rt, &o, &st).await;
        let _ = std::fs::remove_file(&o.bus);
        r
    });
    match select(c3, main).await {
        Either::Left((r, _)) | Either::Right((r, _)) => r,
    }
}

async fn run_bus(rt: &SharedRuntime, o: &Opts, st: &Arc<State>) -> Result<(), String> {
    // A folder of its own (at-spi in the runtime folder) is the user's alone; a given one keeps its mode.
    if let Some(d) = o.bus.parent().filter(|d| !d.exists()) {
        std::fs::create_dir_all(d).map_err(io("bus folder"))?;
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700)).map_err(io("bus folder mode"))?;
    }
    clear_stale(&o.bus)?;
    let addr = format!("unix:path={}", o.bus.display());
    let mut bus = busd::bus::Bus::for_address(Some(&addr)).await.map_err(|e| format!("starting the bus at {addr}: {e}"))?;
    std::fs::set_permissions(&o.bus, std::fs::Permissions::from_mode(0o600)).map_err(io("bus socket mode"))?;
    let running = pin!(bus.run());
    let up = pin!(async {
        let conn = zbus::connection::Builder::address(addr.as_str())
            .name(atspi::REGISTRY_NAME)
            .serve_at(atspi::ROOT_PATH, Socket { apps: st.apps.clone() })
            .serve_at(atspi::REGISTRY_PATH, Registry)
            .serve_at(atspi::DEC_PATH, DeviceEvents)
            .method_timeout(Duration::from_secs(3))
            .build()
            .await
            .map_err(|e| format!("the registry on {addr}: {e}"))?;
        let _ = st.bus.set(Bus::new(conn));
        eprintln!("atspid: the accessibility bus runs at {addr}");
        while switched_on(&o.settings).await {
            rt.sleep(o.look).await;
        }
        eprintln!("atspid: switched off in the settings; ending");
        Ok::<(), String>(())
    });
    match select(running, up).await {
        Either::Left((r, _)) => r.map_err(|e| format!("the bus stopped: {e}")),
        Either::Right((r, _)) => r,
    }
}

async fn accept(st: Arc<State>, l: UnixListener<Shared>, uid: u32, peers: Vec<PathBuf>) -> Result<(), String> {
    loop {
        let (s, _) = l.accept().await.map_err(io("C3 accept"))?;
        let who = match rustix::net::sockopt::socket_peercred(&s) {
            Ok(c) => {
                let exe = crate::peer::program(&s, c.pid.as_raw_pid());
                crate::peer::allowed(c.uid.as_raw(), uid, exe.as_deref(), &peers)
            }
            Err(e) => Err(format!("Wer fragt, sagt das System nicht ({e})")),
        };
        match who {
            Ok(()) => zruntime::spawn(conn(st.clone(), s)).detach(),
            Err(why) => zruntime::spawn(refuse(s, why)).detach(),
        }
    }
}

/// One answer for a peer that may not ask, then the connection ends. Its question is read first, so its write
/// does not meet a closed socket and it gets the reason instead of a broken pipe.
async fn refuse(s: UnixStream<Shared>, why: String) {
    let (r, mut w) = s.split();
    let mut line = String::new();
    let _ = BufReader::new(r).take(c3::MAX_LINE as u64 + 1).read_line(&mut line).await;
    let line = format!("{}\n", serde_json::json!({"id": null, "ok": false, "error": why}));
    let _ = w.write_all(line.as_bytes()).await;
}

async fn conn(st: Arc<State>, s: UnixStream<Shared>) {
    let (r, mut w) = s.split();
    let mut r = BufReader::new(r);
    loop {
        let mut line = String::new();
        let n = match (&mut r).take(c3::MAX_LINE as u64 + 1).read_line(&mut line).await {
            Ok(n) => n,
            Err(_) => return,
        };
        if n == 0 {
            return;
        }
        let answer = if line.len() > c3::MAX_LINE || !line.ends_with('\n') && n > c3::MAX_LINE {
            serde_json::json!({"id": null, "ok": false, "error": "Die Anfrage ist zu lang"})
        } else {
            c3::handle(&st, line.trim()).await
        };
        let mut out = answer.to_string();
        out.push('\n');
        if w.write_all(out.as_bytes()).await.is_err() {
            return;
        }
    }
}
