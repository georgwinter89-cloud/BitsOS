//! In-kernel tests of MC-4b: `install_kernel` (RB-6's tool) the way the desk uses it, over MCP (a loopback server
//! of the test's own with the kernel's real tool list, JSON-RPC over TCP), with the bundle served by a small HTTP
//! server thread on 127.0.0.1 (as `host/mcp/install.mjs` serves it on the Gaming-PC). A synthetic bundle: a small
//! ELF that passes every install check and a one-file newc archive.
//!
//! - `mc.install.dry_run` (Safe, rerunnable, writes nothing on the stick; on the Mini-PC over MCP too): the tool
//!   is registered (owner RB, Change); a dry run plans the next generation number and names both hashes; the
//!   protocol refusals (missing or wrong-typed arguments are invalid params, R9 and form refusals, a guarded
//!   CONFIG line, a 404 bundle) all say "nothing on the stick changed"; the stick's generations, PIN and
//!   CONFIG.TXT are the same before and after.
//! - `mc.install.paths_dry_run` (Safe, rerunnable): the same dry run from files uploaded with `write_file`
//!   (`kernel_path`/`initrd_path`), the exact byte counts in the answer, the refusals of that source.
//! - `mc.install.over_mcp` (QemuOnly, writes the stick image of this run and puts it back): a real install over
//!   MCP (generation N, NEW, both files read back), the files on the stick read back over MCP (`list_dir`,
//!   `read_file` below /boot), `reboot` (dry run) names N's one try as the next start. The generations made are
//!   removed at the end, also when the test fails half way.
//! - `mc.install.paths_over_mcp` (QemuOnly): a real install from uploaded files, byte for byte on the stick.
//!
//! The two path tests skip with a visible reason (`NO_PATHS`) while RB's tool lacks the paths (MC-4b change
//! request 1: plan/notes/mc-MC-4b-rb-paths.patch); once the desk applies it they run and must pass.

use super::files::Fnv;
use crate::kernel::SEC;
use crate::testclient::{Client, with_server};
use crate::{Json, Safety, TOOLS, b64};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use bootinfo::layout::KERNEL_BASE;
use core::sync::atomic::{AtomicBool, Ordering};
use fs_fat::sysapi::Generation;
use ktest::{TestResult, kassert, ktest};
use net::{Ipv4, SockAddr};

/// The test's MCP server and its HTTP server (the next ones of 16 when taken); MC range 25000-25499.
const PORT: u16 = 25110;
const HTTP_PORT: u16 = 25130;

fn fnv(d: &[u8]) -> String {
    let mut h = Fnv::new();
    h.update(d);
    format!("{:#018x}", h.0)
}

/// An x86_64 EXEC ELF at the kernel base with text (R X), rodata (R, with the BitsOS signature the install checks
/// look for) and data+bss (RW), each on pages of its own.
fn kernel(tag: u8) -> Vec<u8> {
    let text: Vec<u8> = (0..6000u32).map(|i| (i as u8).wrapping_mul(29) ^ tag).collect();
    let mut rodata = Vec::from(&b"BitsOS V2 kernel: entry (mc.install ktest)"[..]);
    rodata.push(tag);
    let data = [tag; 64];
    let segs: [(u64, u32, &[u8], u64); 3] =
        [(KERNEL_BASE, 5, &text, text.len() as u64), (KERNEL_BASE + 0x2000, 4, &rodata, rodata.len() as u64), (KERNEL_BASE + 0x3000, 6, &data, 0x2000)];
    let mut f = vec![0u8; 0x1000];
    f[0..4].copy_from_slice(b"\x7fELF");
    (f[4], f[5], f[6]) = (2, 1, 1);
    f[16..18].copy_from_slice(&2u16.to_le_bytes());
    f[18..20].copy_from_slice(&0x3Eu16.to_le_bytes());
    f[20..24].copy_from_slice(&1u32.to_le_bytes());
    f[24..32].copy_from_slice(&(KERNEL_BASE + 0x40).to_le_bytes());
    f[32..40].copy_from_slice(&64u64.to_le_bytes());
    f[52..54].copy_from_slice(&64u16.to_le_bytes());
    f[54..56].copy_from_slice(&56u16.to_le_bytes());
    f[56..58].copy_from_slice(&(segs.len() as u16).to_le_bytes());
    for (i, (vaddr, flags, d, memsz)) in segs.iter().enumerate() {
        let off = f.len() as u64;
        let h = 64 + i * 56;
        f[h..h + 4].copy_from_slice(&1u32.to_le_bytes());
        f[h + 4..h + 8].copy_from_slice(&flags.to_le_bytes());
        f[h + 8..h + 16].copy_from_slice(&off.to_le_bytes());
        f[h + 16..h + 24].copy_from_slice(&vaddr.to_le_bytes());
        f[h + 24..h + 32].copy_from_slice(&vaddr.to_le_bytes());
        f[h + 32..h + 40].copy_from_slice(&(d.len() as u64).to_le_bytes());
        f[h + 40..h + 48].copy_from_slice(&memsz.to_le_bytes());
        f[h + 48..h + 56].copy_from_slice(&0x1000u64.to_le_bytes());
        f.extend_from_slice(d);
        f.resize(f.len().next_multiple_of(0x1000), 0);
    }
    f
}

fn newc(out: &mut Vec<u8>, name: &str, body: &[u8], mode: u32) {
    let fields = [1, mode, 0, 0, 1, 0, body.len() as u32, 0, 0, 0, 0, name.len() as u32 + 1, 0];
    out.extend_from_slice(b"070701");
    for v in fields {
        out.extend_from_slice(format!("{v:08x}").as_bytes());
    }
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    out.resize(out.len().next_multiple_of(4), 0);
    out.extend_from_slice(body);
    out.resize(out.len().next_multiple_of(4), 0);
}

fn initrd(tag: u8) -> Vec<u8> {
    let mut v = Vec::new();
    newc(&mut v, "mc4b/ktest", &[tag; 1500], 0o100644);
    newc(&mut v, "TRAILER!!!", b"", 0);
    v
}

/// A small HTTP server thread on 127.0.0.1 answering GET <path> from `routes` (else 404), one request per
/// connection, until dropped.
/// A small HTTP server thread on 127.0.0.1 answering GET of its routes (MC-push's ktests use it too).
pub(crate) struct Http {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<sched::JoinHandle>,
}

impl Http {
    fn start(routes: Vec<(String, Vec<u8>)>) -> Result<Http, String> {
        Self::start_on(HTTP_PORT, routes)
    }
    /// The server on the first free port of the 16 from `base`.
    pub(crate) fn start_on(base: u16, routes: Vec<(String, Vec<u8>)>) -> Result<Http, String> {
        let sock = net::stack().ok_or("no network stack")?;
        let (l, port) = (0..16u16)
            .find_map(|i| sock.tcp_listen(SockAddr { ip: Ipv4::LOCALHOST, port: base + i }, 4).ok().map(|l| (l, base + i)))
            .ok_or("no free port for the HTTP server")?;
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        let thread = sched::spawn("mc4b-http", sched::Class::Normal, move || {
            while !st.load(Ordering::Acquire) {
                let Ok(c) = l.accept(Some(SEC / 5)) else { continue };
                let mut req = Vec::new();
                let mut buf = [0u8; 512];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") && req.len() < 8192 {
                    match c.read(&mut buf, Some(5 * SEC)) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let line = String::from_utf8_lossy(&req);
                let path = line.split(' ').nth(1).unwrap_or("/");
                match routes.iter().find(|r| r.0 == path) {
                    Some((_, body)) => {
                        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                        if c.write_all(head.as_bytes(), Some(5 * SEC)).is_ok() {
                            let _ = c.write_all(body, Some(10 * SEC));
                        }
                    }
                    None => {
                        let _ = c.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", Some(5 * SEC));
                    }
                }
                c.close();
            }
        })
        .map_err(|e| format!("spawn: {e:?}"))?;
        Ok(Http { port, stop, thread: Some(thread) })
    }
    pub(crate) fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

impl Drop for Http {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.join_timeout(5 * SEC);
        }
        sched::reap();
    }
}

/// The stick as bitsboot and the install see it: generations, PIN, CONFIG.TXT.
type Snapshot = (Vec<Generation>, Option<u32>, Option<Vec<u8>>);

fn snapshot() -> Result<Snapshot, String> {
    fs_fat::stick::with_sys(|s| -> Result<Snapshot, i64> {
        let config = match s.ns.read_file(&format!("{}/BITSOS2/CONFIG.TXT", s.mount), fs_fat::protect::CONFIG_MAX as usize) {
            Ok(t) => Some(t),
            Err(uapi::errno::ENOENT) => None,
            Err(e) => return Err(e),
        };
        Ok((s.generations()?, s.pin()?, config))
    })
    .map_err(|e| format!("the stick: errno {e}"))?
    .map_err(|e| format!("listing the stick: errno {e}"))
}

/// Ok(None): the test skips (no stick in this run).
fn stick() -> Result<Option<Arc<fs_fat::Volume>>, String> {
    match fs_fat::stick::wait_mounted(40_000) {
        Some(v) => Ok(Some(v)),
        None => match fs_fat::stick::last_reason() {
            Some(r) => Err(format!("the boot stick was not mounted: {r}")),
            None => Ok(None),
        },
    }
}

fn has_paths() -> bool {
    crate::find("install_kernel").is_some_and(|t| t.input_schema.contains("\"kernel_path\""))
}

fn json_str(b: &[u8]) -> String {
    let mut s = String::new();
    crate::json::escape_into(&String::from_utf8_lossy(b), &mut s);
    s
}

/// Writes `data` to `path` over MCP in pieces below the request limit (as host/mcp/mcp-call.mjs `upload` does).
fn upload(c: &mut Client, path: &str, data: &[u8]) -> TestResult {
    const PIECE: usize = 128 * 1024;
    for (i, ch) in data.chunks(PIECE).enumerate() {
        let mut s = String::new();
        b64::encode_into(ch, &mut s);
        let how = if i == 0 { String::from(r#""parents":true"#) } else { format!(r#""offset":{}"#, i * PIECE) };
        c.ok("write_file", &format!(r#"{{"path":"{path}","data":"{s}",{how}}}"#))?;
    }
    Ok(())
}

fn refused(c: &mut Client, args: &str, want: &[&str]) -> TestResult {
    let r = c.call("install_kernel", args)?;
    kassert!(
        r.error && want.iter().all(|w| r.text.contains(w)) && r.text.contains("nothing on the stick changed"),
        "install_kernel {args}: want an error with {want:?} and 'nothing on the stick changed', got {:?} {}",
        r.rpc_error,
        r.text
    );
    Ok(())
}

fn invalid(c: &mut Client, args: &str) -> TestResult {
    let r = c.call("install_kernel", args)?;
    kassert!(r.rpc_error == Some(-32602), "install_kernel {args}: want invalid params (-32602), got {:?} {}", r.rpc_error, r.text);
    Ok(())
}

fn scratch(name: &str) -> String {
    let p = format!("/tmp/mc4b-{name}");
    let ns = vfs::root_ns();
    let _ = ns.remove_tree(&p);
    let _ = ns.mkdir_p(&p);
    p
}

fn install_dry_run() -> TestResult {
    let t = crate::find("install_kernel").ok_or("install_kernel is not registered (RB-6 is not in this kernel)")?;
    kassert!(t.owner == "RB" && t.safety == Safety::Change, "install_kernel: owner {} safety {:?}", t.owner, t.safety);
    if stick()?.is_none() {
        return ktest::skip("no boot stick in this run (no USB disk recognised as the stick)");
    }
    let before = snapshot()?;
    let top = before.0.iter().map(|g| g.n).max().unwrap_or(0);
    let (k, i) = (kernel(7), initrd(7));
    let http = Http::start(vec![(String::from("/b/KERNEL"), k.clone()), (String::from("/b/INITRD"), i.clone())])?;
    let r = with_server(PORT, TOOLS.entries(), |c| {
        // The dry run: planned as max+1, both hashes named, nothing written.
        let a = format!(r#"{{"url":"{}","dry_run":true,"allow_no_fallback":true,"timeout_ms":60000}}"#, http.url("/b/"));
        let t = c.ok("install_kernel", &a)?;
        kassert!(t.starts_with(&format!("install_kernel (dry run, nothing written): would install generation {} (NEW)", top + 1)), "{t}");
        kassert!(t.contains(&format!("fnv {}", fnv(&k))) && t.contains(&format!("fnv {}", fnv(&i))), "hashes {} {}: {t}", fnv(&k), fnv(&i));
        // The same with a kernel URL and initrd.
        let a = format!(r#"{{"url":"{}","initrd":"{}","dry_run":true,"allow_no_fallback":true}}"#, http.url("/b/KERNEL"), http.url("/b/INITRD"));
        kassert!(c.ok("install_kernel", &a)?.contains(&format!("would install generation {}", top + 1)));
        // Invalid params: nothing fetched, nothing changed.
        invalid(c, "{}")?;
        invalid(c, r#"{"url":5}"#)?;
        invalid(c, &format!(r#"{{"url":"{}"}}"#, http.url("/b/KERNEL")))?;
        invalid(c, &format!(r#"{{"url":"{}","dry_run":"yes"}}"#, http.url("/b/")))?;
        invalid(c, &format!(r#"{{"url":"{}","config":7,"dry_run":true}}"#, http.url("/b/")))?;
        invalid(c, &format!(r#"{{"url":"{}","unpin":1,"dry_run":true}}"#, http.url("/b/")))?;
        // Refusals: R9, the URL form, a 404 bundle, a CONFIG.TXT that turns on a guarded key.
        let r = c.call("install_kernel", r#"{"url":"http://8.8.8.8/b/","dry_run":true}"#)?;
        kassert!(r.error && r.text.starts_with("Refused: ") && r.text.contains("private"), "public address: {}", r.text);
        let r = c.call("install_kernel", r#"{"url":"https://192.168.1.1/b/","dry_run":true}"#)?;
        kassert!(r.error && r.text.starts_with("Refused: "), "https: {}", r.text);
        refused(c, &format!(r#"{{"url":"{}","dry_run":true,"allow_no_fallback":true}}"#, http.url("/none/")), &["404"])?;
        // A CONFIG.TXT that flips usb_msd_write (whatever the stick says now; the last line wins, as FS-7's guard
        // reads it).
        let mut cfg = before.2.clone().unwrap_or_default();
        let on = String::from_utf8_lossy(&cfg).lines().filter_map(|l| l.trim().strip_prefix("usb_msd_write=")).last().is_some_and(|v| v.trim() == "on");
        cfg.extend_from_slice(if on { b"\nusb_msd_write=off\n" } else { b"\nusb_msd_write=on\n" });
        refused(c, &format!(r#"{{"url":"{}","config":{},"dry_run":true,"allow_no_fallback":true}}"#, http.url("/b/"), json_str(&cfg)), &["usb_msd_write"])?;
        Ok(())
    });
    drop(http);
    r?;
    let after = snapshot()?;
    kassert!(after == before, "the stick changed: {:?} -> {:?}", before.0, after.0);
    Ok(())
}
ktest!("mc.install.dry_run", Safe, install_dry_run);

/// The skip of the path tests names what is missing, so the gate's skip list shows it (MC-4b fixer: a visible
/// skip, not a silent pass).
const NO_PATHS: &str = "install_kernel has no kernel_path/initrd_path: apply plan/notes/mc-MC-4b-rb-paths.patch (MC-4b change request 1 to RB)";

/// The dry run from files uploaded with `write_file` (kernel_path/initrd_path), the exact byte counts in the
/// answer, and the refusals of that source; the stick is the same before and after.
fn install_paths_dry_run() -> TestResult {
    if !has_paths() {
        return ktest::skip(NO_PATHS);
    }
    if stick()?.is_none() {
        return ktest::skip("no boot stick in this run (no USB disk recognised as the stick)");
    }
    let before = snapshot()?;
    let top = before.0.iter().map(|g| g.n).max().unwrap_or(0);
    let (k, i) = (kernel(8), initrd(8));
    let d = scratch("paths");
    let r = with_server(PORT, TOOLS.entries(), |c| {
        upload(c, &format!("{d}/KERNEL"), &k)?;
        upload(c, &format!("{d}/INITRD"), &i)?;
        let a = format!(r#"{{"kernel_path":"{d}/KERNEL","initrd_path":"{d}/INITRD","dry_run":true,"allow_no_fallback":true}}"#);
        let t = c.ok("install_kernel", &a)?;
        kassert!(t.contains(&format!("would install generation {} (NEW)", top + 1)), "{t}");
        kassert!(t.contains(&format!("KERNEL {} bytes (", k.len())) && t.contains(&format!("INITRD {} bytes (", i.len())), "exact bytes {} {}: {t}", k.len(), i.len());
        kassert!(t.contains(&format!("fnv {}", fnv(&k))) && t.contains(&format!("fnv {}", fnv(&i))), "hashes: {t}");
        invalid(c, &format!(r#"{{"kernel_path":"{d}/KERNEL","dry_run":true}}"#))?;
        invalid(c, &format!(r#"{{"url":"http://127.0.0.1:1/b/","kernel_path":"{d}/KERNEL","initrd_path":"{d}/INITRD","dry_run":true}}"#))?;
        refused(c, &format!(r#"{{"kernel_path":"{d}/none","initrd_path":"{d}/INITRD","dry_run":true,"allow_no_fallback":true}}"#), &["ENOENT"])?;
        refused(c, &format!(r#"{{"kernel_path":"/dev/null","initrd_path":"{d}/INITRD","dry_run":true,"allow_no_fallback":true}}"#), &["device node"])?;
        Ok(())
    });
    let _ = vfs::root_ns().remove_tree(&d);
    r?;
    let after = snapshot()?;
    kassert!(after == before, "the stick changed: {:?} -> {:?}", before.0, after.0);
    Ok(())
}
ktest!("mc.install.paths_dry_run", Safe, install_paths_dry_run);

/// Puts the stick back in its Drop (also after a failure half way): removes the generations the test made and
/// the OK marker it set.
struct Restore {
    made: Vec<u32>,
    marked_ok: Option<u32>,
}

impl Restore {
    /// The image's generation carries no marker, so an install would replace it as the unconfirmed newest one:
    /// mark it OK for the test (the fallback a real stick has); Drop takes the mark away again.
    fn fallback(before: &Snapshot, top: u32) -> Result<Restore, String> {
        let mut r = Restore { made: Vec::new(), marked_ok: None };
        if !before.0.iter().any(|g| g.ok) {
            fs_fat::stick::with_sys(|s| s.set_marker(top, fs_fat::sysapi::Marker::Ok, true))
                .map_err(|e| format!("the stick: errno {e}"))?
                .map_err(|e| format!("OK on generation {top}: errno {e}"))?;
            r.marked_ok = Some(top);
        }
        Ok(r)
    }
}

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = fs_fat::stick::with_sys(|s| {
            for &n in &self.made {
                if let Err(e) = s.remove_generation(n)
                    && e != uapi::errno::ENOENT
                {
                    klog::event!(Warn, "mcp", "ktest.install_cleanup_failed", generation = n, errno = e);
                }
            }
            if let Some(n) = self.marked_ok
                && let Err(e) = s.set_marker(n, fs_fat::sysapi::Marker::Ok, false)
            {
                klog::event!(Warn, "mcp", "ktest.install_cleanup_failed", generation = n, errno = e; "OK marker");
            }
        });
    }
}

fn read_all(c: &mut Client, path: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let t = c.ok("read_file", &format!(r#"{{"path":"{path}","offset":{}}}"#, out.len()))?;
        let j = Json::parse(&t).map_err(|e| format!("read_file: not JSON ({e:?})"))?;
        out.extend(b64::decode(j.get("data").and_then(Json::as_str).unwrap_or("")).ok_or("read_file: no base64")?);
        if j.get("eof").and_then(Json::as_bool) != Some(false) {
            return Ok(out);
        }
    }
}

fn install_over_mcp() -> TestResult {
    let Some(vol) = stick()? else { return ktest::skip("no boot stick in this run") };
    kassert!(vol.writable(), "the stick is read-only: {}", fs_fat::stick::last_reason().unwrap_or("?"));
    let before = snapshot()?;
    let top = before.0.iter().map(|g| g.n).max().ok_or("the stick holds no generation")?;
    let (k1, i1) = (kernel(1), initrd(1));
    let http = Http::start(vec![(String::from("/a/KERNEL"), k1.clone()), (String::from("/a/INITRD"), i1.clone())])?;
    let mut restore = Restore::fallback(&before, top)?;
    let r = with_server(PORT, TOOLS.entries(), |c| {
        let n = top + 1;
        restore.made.push(n);
        let t = c.ok("install_kernel", &format!(r#"{{"url":"{}","timeout_ms":60000}}"#, http.url("/a/")))?;
        kassert!(t.starts_with(&format!("installed generation {n} (NEW)")) && t.contains("both read back from the stick and equal"), "{t}");
        kassert!(t.contains(&format!("fallback (KERNEL.OLD): generation {top}")) && !t.contains("pruned"), "{t}");
        kassert!(t.contains(&format!("fnv {}", fnv(&k1))) && t.contains(&format!("fnv {}", fnv(&i1))), "{t}");
        kassert!(t.contains(&format!("next boot starts: generation {n} (NEWEST_TRY")), "{t}");
        // The stick through the file tools: the folder, the files byte for byte.
        let l = c.ok("list_dir", &format!(r#"{{"path":"/boot/BITSOS2/SYS/{n}"}}"#))?;
        kassert!(l.contains(" KERNEL") && l.contains(" INITRD") && l.contains(" NEW"), "{l}");
        kassert!(read_all(c, &format!("/boot/BITSOS2/SYS/{n}/KERNEL"))? == k1, "SYS/{n}/KERNEL read over MCP differs");
        kassert!(read_all(c, &format!("/boot/BITSOS2/SYS/{n}/INITRD"))? == i1, "SYS/{n}/INITRD read over MCP differs");
        // reboot names the new generation's one try (read from the stick, MC-4b).
        let t = c.ok("reboot", r#"{"dry_run":true}"#)?;
        kassert!(t.contains(&format!("next start: KERNEL: generation {n} (NEWEST_TRY")), "{t}");
        Ok(())
    });
    drop(http);
    drop(restore);
    r?;
    let after = snapshot()?;
    kassert!(after.0 == before.0 && after.2 == before.2, "the stick was not put back: {:?} -> {:?}", before.0, after.0);
    Ok(())
}
ktest!("mc.install.over_mcp", QemuOnly, install_over_mcp);

/// A real install from files uploaded with `write_file`: generation N from the uploads, exact bytes named,
/// both files on the stick byte for byte; the stick is put back afterwards.
fn install_paths_over_mcp() -> TestResult {
    if !has_paths() {
        return ktest::skip(NO_PATHS);
    }
    let Some(vol) = stick()? else { return ktest::skip("no boot stick in this run") };
    kassert!(vol.writable(), "the stick is read-only: {}", fs_fat::stick::last_reason().unwrap_or("?"));
    let before = snapshot()?;
    let top = before.0.iter().map(|g| g.n).max().ok_or("the stick holds no generation")?;
    let (k, i) = (kernel(2), initrd(2));
    let mut restore = Restore::fallback(&before, top)?;
    let d = scratch("over-paths");
    let r = with_server(PORT, TOOLS.entries(), |c| {
        let n = top + 1;
        upload(c, &format!("{d}/KERNEL"), &k)?;
        upload(c, &format!("{d}/INITRD"), &i)?;
        restore.made.push(n);
        let a = format!(r#"{{"kernel_path":"{d}/KERNEL","initrd_path":"{d}/INITRD","timeout_ms":60000}}"#);
        let t = c.ok("install_kernel", &a)?;
        kassert!(t.starts_with(&format!("installed generation {n} (NEW)")) && t.contains("both read back from the stick and equal"), "{t}");
        kassert!(t.contains(&format!("KERNEL {} bytes (", k.len())) && t.contains(&format!("INITRD {} bytes (", i.len())), "exact bytes: {t}");
        kassert!(read_all(c, &format!("/boot/BITSOS2/SYS/{n}/KERNEL"))? == k, "SYS/{n}/KERNEL differs from the upload");
        kassert!(read_all(c, &format!("/boot/BITSOS2/SYS/{n}/INITRD"))? == i, "SYS/{n}/INITRD differs from the upload");
        Ok(())
    });
    drop(restore);
    let _ = vfs::root_ns().remove_tree(&d);
    r?;
    let after = snapshot()?;
    kassert!(after.0 == before.0 && after.2 == before.2, "the stick was not put back: {:?} -> {:?}", before.0, after.0);
    Ok(())
}
ktest!("mc.install.paths_over_mcp", QemuOnly, install_paths_over_mcp);
