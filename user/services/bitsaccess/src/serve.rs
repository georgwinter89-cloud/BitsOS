//! `bitsaccess serve` (init starts it as uid 0): keeps the log in memory until /data can be written (then on
//! /data), switches the kernel's recording as `access.enabled` says, reads `/dev/bitsos/access` twice a second,
//! keeps the entries (`store`), answers `/run/bitsos/access.sock` for root and the session's user, and registers the MCP tool `access_log` (the kernel
//! offers it only to a client with full access, MC-10). Settings are read again every 10 s and at once when
//! settingsd says an `access.*` key changed (its watch), the days written every 30 s and retention applied every
//! 10 minutes.
//!
//! Until settingsd answered once, nothing is read from the node and the kernel's switch is left alone (it records
//! from boot; its rings keep the first records and count what they cannot keep): what Georg switched off must not
//! be stored because settingsd started late. After `SETTINGS_WAIT` without an answer the defaults hold.

use crate::kernel;
use crate::model::Entry;
use crate::query;
use crate::settings::{self, Settings};
use crate::store::Store;
use crate::{DIR, NODE, SESSION_UID, SOCKET, now_ms};
use mcpreg::{Json, Reply, Safety, Service, Tool};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long the log waits for settingsd before its defaults hold.
pub const SETTINGS_WAIT: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct State {
    pub store: Store,
    pub settings: Settings,
    /// settingsd answered every key once (until then `settings` are the defaults or only partly read).
    pub known: bool,
    /// settingsd said an `access.*` key changed: read them again now.
    pub reload: bool,
}

pub type Shared = Arc<Mutex<State>>;

fn log(m: &str) {
    let line = format!("bitsaccess: {m}\n");
    if fs::OpenOptions::new().write(true).open("/dev/kmsg").and_then(|mut f| f.write_all(line.as_bytes())).is_err() {
        eprint!("{line}");
    }
}

/// Who may ask: root and the session's user.
pub fn allowed(uid: u32) -> bool {
    uid == 0 || uid == SESSION_UID
}

/// Entries a batch keeps: not this service's own (it reads and writes the log), not an ignored program's.
pub fn keep(entries: Vec<Entry>, s: &Settings, own_pid: u32) -> Vec<Entry> {
    entries.into_iter().filter(|e| e.pid != own_pid && !s.ignored(&e.exe)).collect()
}

/// One batch from the node (reads until the kernel's status line; whole lines each read).
fn read_batch(node: &mut impl Read) -> std::io::Result<String> {
    let mut text = String::new();
    let mut buf = vec![0u8; 256 << 10];
    for _ in 0..4096 {
        let n = node.read(&mut buf)?;
        text.push_str(&String::from_utf8_lossy(&buf[..n]));
        if n == 0 || kernel::complete(&text) {
            break;
        }
    }
    Ok(text)
}

fn switch(node: &mut impl Write, on: bool) {
    if let Err(e) = node.write_all(if on { b"on\n" } else { b"off\n" }) {
        log(&format!("could not switch the kernel's recording {}: {e}", if on { "on" } else { "off" }));
    }
}

/// One round of the reader: nothing before the settings are known (or `waited` reached `SETTINGS_WAIT`); then the
/// kernel switched as `access.enabled` says and, when on, one batch read and stored. Answers whether a batch was
/// stored.
pub fn round(state: &Shared, node: &mut (impl Read + Write), me: u32, was_on: &mut Option<bool>, waited: Duration) -> std::io::Result<bool> {
    let Ok((known, on)) = state.lock().map(|s| (s.known, s.settings.enabled)) else { return Ok(false) };
    if !known && waited < SETTINGS_WAIT {
        return Ok(false);
    }
    if *was_on != Some(on) {
        if !known {
            log(&format!("settingsd did not answer in {} s: the defaults hold", SETTINGS_WAIT.as_secs()));
        }
        switch(node, on);
        log(&format!("recording {}", if on { "on" } else { "off (access.enabled)" }));
        *was_on = Some(on);
        if let Ok(mut s) = state.lock() {
            s.store.on = on;
        }
    }
    if !on {
        return Ok(false);
    }
    let b = kernel::parse(&read_batch(node)?, now_ms());
    let Ok(mut s) = state.lock() else { return Ok(false) };
    // Switched off while the batch was read: it is not kept (the next round switches the kernel off).
    if !s.settings.enabled {
        return Ok(false);
    }
    let entries = keep(b.entries, &s.settings, me);
    let roots = s.settings.private_roots.clone();
    s.store.on = b.on;
    s.store.lost = b.lost;
    s.store.add(entries, &roots, now_ms());
    Ok(true)
}

fn reader(state: Shared, mut node: fs::File) {
    let me = std::process::id();
    let started = Instant::now();
    let mut was_on: Option<bool> = None;
    loop {
        if let Err(e) = round(&state, &mut node, me, &mut was_on, started.elapsed()) {
            log(&format!("{NODE}: {e}; trying again in 10 s"));
            std::thread::sleep(Duration::from_secs(10));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// settingsd's watch of `access.*`: every change asks for a new read of the settings (the switch reaches the
/// kernel within a second instead of the 10 s poll).
fn watch_settings(state: Shared) {
    loop {
        if let Ok(s) = UnixStream::connect(settings::socket())
            && let Ok(mut w) = s.try_clone()
            && writeln!(w, r#"{{"op":"watch","prefix":"access."}}"#).is_ok()
        {
            for line in BufReader::new(s).lines() {
                let Ok(line) = line else { break };
                if !line.contains("\"watching\"")
                    && let Ok(mut st) = state.lock()
                {
                    st.reload = true;
                }
            }
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

/// One connection: its peer must be root or the session's user; then one answer line per request line.
pub fn handle(stream: UnixStream, state: &Shared, peer_uid: Option<u32>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let Ok(mut w) = stream.try_clone() else { return };
    if !peer_uid.is_some_and(allowed) {
        let _ = writeln!(w, "{}", Json::obj(vec![("error", Json::str("nur für Georgs Sitzung und root"))]));
        return;
    }
    for line in BufReader::new(stream).lines().take(1000) {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let answer = match state.lock() {
            Ok(s) => query::answer(&s.store, &line, &s.settings.private_roots, now_ms()),
            Err(_) => Json::obj(vec![("error", Json::str("intern"))]).to_string(),
        };
        if writeln!(w, "{answer}").is_err() {
            return;
        }
    }
}

fn listen(state: Shared, path: &str) {
    let _ = fs::remove_file(path);
    let l = match UnixListener::bind(path) {
        Ok(l) => l,
        Err(e) => return log(&format!("{path}: {e}; no answers for the page")),
    };
    // Everyone may connect; the peer's uid decides (SO_PEERCRED).
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o666));
    for s in l.incoming().flatten() {
        let uid = usys::peer_cred(s.as_raw_fd() as usize).ok().map(|c| c.1);
        let st = state.clone();
        let _ = std::thread::Builder::new().name("bitsaccess-conn".into()).spawn(move || handle(s, &st, uid));
    }
}

const TOOL_SCHEMA: &str = r#"{"type":"object","properties":{"op":{"type":"string","enum":["summary","events"],"description":"summary (default): per program its folders, hosts, devices and rights; events: the entries, newest first"},"since_s":{"type":"integer","minimum":1,"description":"how far back, in seconds (default 86400)"},"app":{"type":"string","description":"one program: its path or file name"},"kind":{"type":"string","description":"events: read write create unlink rename exec device connect listen accept netclose setid caps ptrace, or files/net/rights"},"max":{"type":"integer","minimum":1,"maximum":2000,"description":"events: at most this many (default 200)"}}}"#;

pub fn tools() -> Vec<Tool> {
    vec![Tool::new(
        "access_log",
        "Who did what (DB2, Durchblick): per program the folders it read and wrote, the hosts and ports it talked to (bytes per connection), the devices it opened (microphone, camera, sound, input, drives, GPU, USB) and rights it raised, kept on this machine up to 30 days. Times are unix ms. Only offered with Georg's full-access grant.",
        TOOL_SCHEMA,
        Safety::Read,
    )
    .expect("schema is JSON")]
}

/// The tool's answer (`op` summary or events), the socket's ops with the same arguments.
pub fn tool_answer(state: &Shared, args: &Json) -> Reply {
    let op = args.get("op").and_then(Json::as_str).unwrap_or("summary");
    if op != "summary" && op != "events" {
        return Reply::bad_argument("op: summary or events");
    }
    let req = Json::obj(vec![("id", Json::Num(0.0)), ("op", Json::str(op)), ("args", args.clone())]).to_string();
    match state.lock() {
        Ok(s) => Reply::text(query::answer(&s.store, &req, &s.settings.private_roots, now_ms())),
        Err(_) => Reply::text("bitsaccess: internal error".to_string()),
    }
}

fn mcp_loop(state: Shared) {
    let mut delay = 1;
    loop {
        let r = Service::connect(settings::OWNER).and_then(|mut s| {
            s.register(&tools())?;
            log("MCP tool access_log registered");
            delay = 1;
            s.serve(|c, _| {
                // A local program through MCP only for root or the session (the network side is the kernel's gate).
                if c.caller.is_some_and(|p| !allowed(p.uid)) {
                    return Reply::bad_argument("access_log: only root and the session's user");
                }
                if c.name == "access_log" { tool_answer(&state, &c.arguments) } else { Reply::bad_argument(format!("unknown tool {}", c.name)) }
            })
        });
        if let Err(e) = r {
            log(&format!("MCP: {e}; retrying in {delay} s"));
        }
        std::thread::sleep(Duration::from_secs(delay));
        delay = (delay * 2).min(60);
    }
}

/// Whether /data can be written now; `said` keeps the last reason logged.
fn data_ready(said: &mut String) -> bool {
    match bitsjournal::data_state(fs::read_to_string(bitsjournal::DATA_STATE).ok().as_deref()) {
        bitsjournal::DataState::Ready => true,
        bitsjournal::DataState::Pending => false,
        bitsjournal::DataState::Never(why) => {
            if why != *said {
                log(&format!("no writable /data on this boot ({why}): kept in memory only"));
                *said = why;
            }
            false
        }
    }
}

fn prepare(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(dir.join("days"))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

pub fn serve() -> i32 {
    let dir = Path::new(DIR);
    let node = match fs::OpenOptions::new().read(true).write(true).open(NODE) {
        Ok(f) => f,
        Err(e) => {
            log(&format!("{NODE}: {e} (a kernel without the access log?)"));
            return 1;
        }
    };
    let (set, note) = settings::load(&settings::socket());
    if let Some(n) = &note {
        log(&format!("settings: {n}"));
    }
    // The log runs from memory until /data can be written (a boot without /data still shows today).
    let store = Store::in_memory(dir);
    log(&format!("started: enabled={}, keep_days={}, max_mb={}{}", set.enabled, set.keep_days, set.max_mb, if note.is_some() { " (not from settingsd yet: waiting)" } else { "" }));
    let state: Shared = Arc::new(Mutex::new(State { store, settings: set, known: note.is_none(), reload: false }));
    let st = state.clone();
    let _ = std::thread::Builder::new().name("bitsaccess-watch".into()).spawn(move || watch_settings(st));
    let st = state.clone();
    let _ = std::thread::Builder::new().name("bitsaccess-read".into()).spawn(move || reader(st, node));
    let st = state.clone();
    let _ = std::thread::Builder::new().name("bitsaccess-sock".into()).spawn(move || listen(st, SOCKET));
    let st = state.clone();
    let _ = std::thread::Builder::new().name("bitsaccess-mcp".into()).spawn(move || mcp_loop(st));
    let (mut last_set, mut last_flush, mut last_retain) = (Instant::now(), Instant::now(), None::<Instant>);
    // Until settingsd answered once without a complaint (it may start after this service), asked every second.
    let (mut on_disk, mut said, mut last_note) = (false, String::new(), note);
    loop {
        if !on_disk && data_ready(&mut said) {
            match prepare(dir) {
                Ok(()) => {
                    on_disk = true;
                    if let Ok(mut s) = state.lock() {
                        s.store.load_disk();
                        log(&format!("{DIR}: {} days kept", s.store.days.len()));
                    }
                }
                Err(e) => log(&format!("{DIR}: {e}; kept in memory only")),
            }
        }
        std::thread::sleep(Duration::from_millis(250));
        let asked = state.lock().map(|mut s| std::mem::take(&mut s.reload)).unwrap_or(false);
        if asked || last_set.elapsed() >= Duration::from_secs(if last_note.is_some() { 1 } else { 10 }) {
            last_set = Instant::now();
            let (set, note) = settings::load(&settings::socket());
            if note != last_note {
                log(&format!("settings: {}", note.as_deref().unwrap_or("read")));
            }
            if let Ok(mut s) = state.lock() {
                s.settings = set;
                s.known |= note.is_none();
            }
            last_note = note;
        }
        if !on_disk {
            if let Ok(mut s) = state.lock() {
                let now = now_ms();
                s.store.forget_before(now.saturating_sub(86_400_000));
            }
            continue;
        }
        if last_retain.is_none_or(|t| t.elapsed() >= Duration::from_secs(600))
            && let Ok(mut s) = state.lock()
        {
            last_retain = Some(Instant::now());
            let (keep, max) = (s.settings.keep_days, s.settings.max_bytes());
            let gone = s.store.retain(now_ms(), keep, max);
            if !gone.is_empty() {
                log(&format!("retention: removed the days {} (access.keep_days={keep}, access.max_mb={})", gone.join(" "), max >> 20));
            }
        }
        if last_flush.elapsed() >= Duration::from_secs(30)
            && let Ok(mut s) = state.lock()
        {
            last_flush = Instant::now();
            if let Err(e) = s.store.flush() {
                log(&format!("{DIR}: not written: {e} (kept in memory, tried again)"));
            }
        }
    }
}
