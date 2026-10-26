//! MCP tool `install_kernel` (RB registers it, like `reload`; MC-4b's part is the host side): fetch the bundle
//! over http from the LAN (Reload's URL rules, R9), check it, install it as a new generation on the identified
//! boot stick, answer with what the stick holds and what the next boot starts.

use super::{InstallError, LOCK, Opts, Report, check, confirm, install_on};
use crate::tool::{Busy, bundle_from, opt_bool, opt_str};
use crate::stage;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult, mcp_tool};
use net::http::{self, HttpError};

const SEC: u64 = 1_000_000_000;

/// Fetches one whole file into memory (`limit` bytes at most; sized from Content-Length when the server sends it).
fn fetch(what: &str, url: &str, limit: u64, deadline_ns: u64) -> Result<Vec<u8>, ToolError> {
    let sock = net::stack().ok_or_else(|| ToolError::Failed(String::from("no network stack")))?;
    // (bytes, out of memory): shared by the head and the body callbacks.
    let buf = core::cell::RefCell::new((Vec::<u8>::new(), false));
    let r = http::get_to_with(
        sock,
        url,
        &stage::options(limit, deadline_ns),
        &mut |h| {
            let mut b = buf.borrow_mut();
            let want = h.content_length.unwrap_or(0) as usize;
            if b.0.try_reserve_exact(want).is_err() {
                b.1 = true;
                return Err(());
            }
            Ok(())
        },
        &mut |d| {
            let mut b = buf.borrow_mut();
            if b.0.try_reserve(d.len()).is_err() {
                b.1 = true;
                return Err(());
            }
            b.0.extend_from_slice(d);
            Ok(())
        },
    );
    let (out, short) = buf.into_inner();
    match r {
        Ok(_) => Ok(out),
        Err(_) if short => Err(ToolError::Refused(format!("{what}: {url} does not fit in memory now ({} KiB read)", out.len() >> 10))),
        Err(HttpError::Status(c)) => Err(ToolError::Failed(format!("{what}: {url} answered HTTP {c}"))),
        Err(e) => Err(ToolError::Failed(stage::http_text(what, url, &e))),
    }
}

/// B6: a failed download is logged; the answer says the stick was not touched (nothing is written before both
/// files are in memory and checked).
fn fetch_failed(e: ToolError) -> ToolError {
    let (refused, t) = match e {
        ToolError::Refused(t) => (true, t),
        ToolError::Failed(t) | ToolError::BadArgument(t) => (false, t),
    };
    klog::event!(Warn, "kexec", "install.fetch_failed", reason = t.as_str(); "nothing on the stick changed");
    let t = format!("{t} (nothing on the stick changed)");
    if refused { ToolError::Refused(t) } else { ToolError::Failed(t) }
}

fn mib(b: u64) -> String {
    format!("{}.{} MiB", b >> 20, ((b & ((1 << 20) - 1)) * 10) >> 20)
}

fn answer(r: &Report, dry: bool) -> String {
    let p = &r.plan;
    let mut t = String::new();
    let (k, i) = (r.kernel.clone().unwrap_or_default(), r.initrd.clone().unwrap_or_default());
    if dry {
        t.push_str(&format!("install_kernel (dry run, nothing written): would install generation {} (NEW)", p.number));
    } else {
        t.push_str(&format!("installed generation {} (NEW): \\BITSOS2\\SYS\\{}\\", p.number, p.number));
    }
    t.push_str(&format!(
        "\nKERNEL {} bytes ({}, fnv {:#018x}, entry {:#x}, image {} bytes), INITRD {} bytes ({}, fnv {:#018x}, {} entries){}",
        k.bytes,
        mib(k.bytes),
        k.fnv,
        k.entry,
        k.span,
        i.bytes,
        mib(i.bytes),
        i.fnv,
        i.entries,
        if dry { "" } else { "; both read back from the stick and equal" }
    ));
    if let Some(old) = p.replaced {
        t.push_str(&format!("\nreplaced: the unconfirmed generation {old} {}", if dry { "would be removed" } else { "was removed" }));
    }
    if !p.prune.is_empty() {
        t.push_str(&format!("\npruned (sys_keep={}): {:?}", r.sys_keep.unwrap_or(0), p.prune));
    }
    match p.fallback {
        Some(f) => t.push_str(&format!("\nfallback (KERNEL.OLD): generation {f}")),
        None => t.push_str("\nfallback: NONE (allow_no_fallback): a failing new kernel starts again at every boot"),
    }
    if r.config_written {
        t.push_str("\nCONFIG.TXT written");
    }
    if !dry {
        t.push_str(&format!("\nnext boot starts: {}", r.next_boot));
        let need = crate::boot::config("boot_confirm_s").and_then(|v| v.parse::<u64>().ok()).unwrap_or(confirm::DEFAULT_S).clamp(confirm::MIN_S, confirm::MAX_S);
        t.push_str(&format!(
            "\nThe new generation starts at the next reboot as its one try; it confirms itself (OK) after MCP has answered over {need} s with no pause over {} s (keep calling MCP, e.g. reload_info every few seconds; do not Reload before that); otherwise the boot after it starts the fallback.",
            confirm::gap_for(need * SEC) / SEC
        ));
    }
    for w in &r.warnings {
        t.push_str(&format!("\nwarning: {w}"));
    }
    t
}

/// Where the bundle comes from: fetched over http from the LAN, or files already on BitsOS (uploaded with
/// `write_file`; MC-4b).
enum Source<'a> {
    Url { kernel: String, initrd: String },
    Paths { kernel: &'a str, initrd: &'a str },
}

fn source(args: &Json) -> Result<Source<'_>, ToolError> {
    let (kp, ip) = (opt_str(args, "kernel_path")?, opt_str(args, "initrd_path")?);
    if kp.is_none() && ip.is_none() {
        let b = bundle_from(args)?;
        let initrd = b.initrd.clone().ok_or_else(|| {
            ToolError::BadArgument(String::from("an install needs both files: url ending in / (KERNEL + INITRD) or url + initrd"))
        })?;
        return Ok(Source::Url { kernel: b.kernel, initrd });
    }
    if opt_str(args, "url")?.is_some() || opt_str(args, "initrd")?.is_some() {
        return Err(ToolError::BadArgument(String::from("either url (+ initrd) or kernel_path + initrd_path, not both")));
    }
    match (kp, ip) {
        (Some(kernel), Some(initrd)) => Ok(Source::Paths { kernel, initrd }),
        _ => Err(ToolError::BadArgument(String::from("an install needs both files: kernel_path and initrd_path"))),
    }
}

pub(super) fn install_kernel(ctx: &ToolContext, args: &Json) -> ToolResult {
    install_with(ctx, args, false)
}

/// RB-9: `install_system` = `install_kernel` whose initramfs must also hold init and the early services.
fn install_system(ctx: &ToolContext, args: &Json) -> ToolResult {
    install_with(ctx, args, true)
}

pub(super) fn install_with(ctx: &ToolContext, args: &Json, system: bool) -> ToolResult {
    let tool = if system { "install_system" } else { "install_kernel" };
    let src = source(args)?;
    let (from, initrd_from) = match &src {
        Source::Url { kernel, initrd } => (kernel.as_str(), initrd.as_str()),
        Source::Paths { kernel, initrd } => (*kernel, *initrd),
    };
    let config = match args.get("config") {
        None | Some(Json::Null) => None,
        Some(v) => Some(v.as_str().ok_or_else(|| ToolError::BadArgument(String::from("config must be a string")))?.as_bytes().to_vec()),
    };
    let o = Opts {
        unpin: opt_bool(args, "unpin")?.unwrap_or(false),
        allow_no_fallback: opt_bool(args, "allow_no_fallback")?.unwrap_or(false),
        dry_run: opt_bool(args, "dry_run")?.unwrap_or(false),
    };
    // Before anything is fetched: the stick must be there and writable (a dry run only needs it readable).
    let refuse = |t: String| {
        klog::event!(Warn, "kexec", "install.refused", reason = t.as_str(); "nothing fetched, nothing on the stick changed");
        ToolError::Refused(format!("{t} (nothing on the stick changed)"))
    };
    let vol = fs_fat::stick::volume().ok_or_else(|| {
        refuse(format!(
            "no boot stick mounted at /boot ({}): install_kernel writes only the identified boot stick",
            fs_fat::stick::last_reason().unwrap_or("none recognised")
        ))
    })?;
    if !o.dry_run && !vol.writable() {
        return Err(refuse(format!(
            "the boot stick is read-only ({}): install_kernel writes only the identified boot stick (R8b)",
            fs_fat::stick::last_reason().unwrap_or("?")
        )));
    }
    let Some(_busy) = Busy::take() else {
        return Err(refuse(String::from(
            "a Reload, an install_kernel or the boot confirmation's stick write is in progress; try again in a few seconds",
        )));
    };
    klog::event!(Info, "kexec", "install.requested", url = from, initrd = initrd_from, config = config.is_some(), unpin = o.unpin,
        allow_no_fallback = o.allow_no_fallback, dry_run = o.dry_run);
    // Kernel + initramfs + the page cache's copy while they are written (the read-back goes piece by piece), with
    // room to spare: a quarter of what the running kernel can spare, for both files.
    let limit = stage::budget() / 4;
    let get = |what: &str, at: &str, max: u64| match &src {
        Source::Url { .. } => fetch(what, at, max, ctx.deadline_ns),
        // MC's read rules (B3 path check, regular files only, whole or not at all).
        Source::Paths { .. } => mcp::tools::files::read_whole(ctx, tool, at, max),
    };
    let kernel = get("kernel", from, limit.min(check::KERNEL_FILE_MAX)).map_err(fetch_failed)?;
    let initrd =
        get("initramfs", initrd_from, limit.saturating_sub(kernel.len() as u64).min(check::INITRD_FILE_MAX)).map_err(fetch_failed)?;
    klog::event!(Info, "kexec", "install.fetched", kernel_bytes = kernel.len(), initrd_bytes = initrd.len());
    let services = if system {
        let s = check::services(&initrd).map_err(|t| {
            klog::event!(Warn, "kexec", "install.refused", reason = t.as_str(), tool = tool; "nothing on the stick changed");
            ToolError::Refused(format!("{t} (nothing on the stick changed)"))
        })?;
        s.iter().map(|(n, b)| format!("/{n} ({b} bytes)")).collect::<Vec<_>>().join(", ")
    } else {
        String::new()
    };
    let _l = LOCK.lock_timeout(60 * SEC).ok_or_else(|| {
        klog::event!(Warn, "kexec", "install.lock_timeout", waited_s = 60u64; "nothing on the stick changed");
        ToolError::Failed(String::from("the stick stayed busy for over a minute (nothing on the stick changed)"))
    })?;
    let r = fs_fat::stick::with_sys(|s| install_on(&super::FatStick(s), &kernel, &initrd, config.as_deref(), &o))
        .map_err(|e| ToolError::Refused(format!("the boot stick went away: {}", super::errno_name(e))))?;
    match r {
        Ok(r) => {
            if let (false, Some(k)) = (o.dry_run, &r.kernel) {
                super::system_stick::note_kernel_id(r.plan.number, k.bytes, k.fnv);
            }
            let mut t = answer(&r, o.dry_run);
            if system {
                t = t.replacen("install_kernel (dry run", "install_system (dry run", 1).replacen("installed generation", "installed system generation", 1);
                t.push_str(&format!("\ninit and the early services in the initramfs: {services}"));
            }
            Ok(vec![ToolOutput::Text(t)])
        }
        Err(InstallError::Refused(t)) => {
            klog::event!(Warn, "kexec", "install.refused", reason = t.as_str(); "nothing on the stick changed");
            Err(ToolError::Refused(format!("{t} (nothing on the stick changed)")))
        }
        Err(InstallError::Failed(t)) => Err(ToolError::Failed(t)),
    }
}

mcp_tool!(
    name: "install_kernel",
    owner: "RB",
    safety: Change,
    description: "Installs a kernel + initramfs on the boot stick as a new system generation \\BITSOS2\\SYS\\<n>\\ (marked NEW; the unconfirmed newest one is replaced, old ones pruned to sys_keep). The bundle is fetched over http from the LAN (url) or read from files already on BitsOS (kernel_path + initrd_path, uploaded with write_file) and must pass bitsboot's own checks; both files are read back from the stick and compared. Nothing restarts: the new generation starts at the next reboot as its one try and confirms itself after MCP has answered for 2 minutes, else bitsboot falls back to the newest confirmed generation. url: a directory ending in / (KERNEL + INITRD below it) or the kernel file with initrd. config: a whole new CONFIG.TXT (refused if it adds or changes nvme_write/stick_write/usb_msd_write or drops the token). unpin: remove SYS\\PIN. allow_no_fallback: install although no confirmed generation stays. dry_run: fetch, check and plan only.",
    schema: r#"{"type":"object","properties":{"url":{"type":"string","description":"http://<LAN IPv4>[:port]/dir/ (KERNEL + INITRD) or .../KERNEL"},"initrd":{"type":"string","description":"http URL of the initramfs (with a kernel-file url)"},"kernel_path":{"type":"string","description":"instead of url: absolute path of a kernel file on BitsOS (uploaded with write_file)"},"initrd_path":{"type":"string","description":"with kernel_path: absolute path of the initramfs on BitsOS"},"config":{"type":"string","description":"new content of \\BITSOS2\\CONFIG.TXT (optional)"},"unpin":{"type":"boolean"},"allow_no_fallback":{"type":"boolean"},"dry_run":{"type":"boolean"},"timeout_ms":{"type":"integer","default":300000}}}"#,
    handler: install_kernel,
);

mcp_tool!(
    name: "install_system",
    owner: "RB",
    safety: Change,
    description: "Installs a whole system generation (B1): install_kernel with the extra check that the initramfs holds init and the early services (/bin/init, /bin/settingsd, /bin/bitssnap as executable files). Same arguments and answer as install_kernel; the new generation starts at the next reboot as its one try and confirms itself after 2 minutes of MCP, else bitsboot falls back by itself. system_list shows the generations, system_pin goes back to one.",
    schema: r#"{"type":"object","properties":{"url":{"type":"string","description":"http://<LAN IPv4>[:port]/dir/ (KERNEL + INITRD) or .../KERNEL"},"initrd":{"type":"string","description":"http URL of the initramfs (with a kernel-file url)"},"kernel_path":{"type":"string","description":"instead of url: absolute path of a kernel file on BitsOS (uploaded with write_file)"},"initrd_path":{"type":"string","description":"with kernel_path: absolute path of the initramfs on BitsOS"},"config":{"type":"string","description":"new content of \\BITSOS2\\CONFIG.TXT (optional)"},"unpin":{"type":"boolean"},"allow_no_fallback":{"type":"boolean"},"dry_run":{"type":"boolean"},"timeout_ms":{"type":"integer","default":300000}}}"#,
    handler: install_system,
);
