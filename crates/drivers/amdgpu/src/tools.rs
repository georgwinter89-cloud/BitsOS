//! MCP tools of GX (`dcn_snapshot`/`dcn_writes` safety Read: they only read registers and files):
//! - `dcn_snapshot`: the device object, the gates' verdict, the KMS view and every snapshot register as JSON;
//!   with `compare` (a file path on the device) or `reference` (the text itself) the diff against it instead.
//! - `dcn_writes`: the `DcnRegs` write log (count, mismatches, the most recent writes).
//! - `dcn_flip` (GX-3, safety Change: it flips): state, the FlipPace bar (flip or copy), the CRC check.
//! - `display_mode` (GX-4, safety Change: it switches the mode): list and state, `set`, `confirm`, `undo`.

use crate::dcn::{self, DcnRegs};
use crate::gpu::{self, Gpu};
use crate::{kms, snapshot};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use console::Console;
use devmodel::display::DisplayDevice;
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult};

/// A reference file is a snapshot of a few hundred registers (~10 KB); 1 MiB bounds one call's memory.
const REFERENCE_MAX: u64 = 1 << 20;

/// The text answer for a snapshot (pure: host-tested with fake registers).
pub fn snapshot_text(head: &[String], attach: &str, topo: &kms::Topology, json: &str) -> String {
    let mut s = String::new();
    for l in head {
        s += l;
        s.push('\n');
    }
    s += "attach: ";
    s += attach;
    s.push('\n');
    for l in topo.lines() {
        s += &l;
        s.push('\n');
    }
    s += json;
    s.push('\n');
    s
}

fn attach_text(g: &Gpu) -> String {
    match g.check_display() {
        Ok(a) => format!(
            "ok: A0 GPU {:#x}, A1 GPU {:#x}, {} bytes, {} pipe(s) {:?}, DMCUB {}, {} window(s), settle {:?}",
            a.a0,
            a.a1,
            a.bytes,
            a.pipes.len(),
            a.pipes.iter().map(|p| (p.hubp, p.otg)).collect::<alloc::vec::Vec<_>>(),
            if a.dmcub_running { "running" } else { "off" },
            a.windows.len(),
            a.settle
        ),
        Err(e) => format!("refused, {e}"),
    }
}

/// The parsed arguments of `dcn_snapshot`: the scope and the reference text, if any (`compare` read from a file).
pub struct Args {
    pub wide: bool,
    pub reference: Option<String>,
}

/// Checks the arguments and reads a `compare` file (before the engine is looked at: QEMU runs this part too).
pub fn parse_args(
    args: &Json,
    read_file: impl FnOnce(&str) -> Result<alloc::vec::Vec<u8>, ToolError>,
) -> Result<Args, ToolError> {
    let bad = |n: &str| ToolError::BadArgument(String::from(n));
    let wide = match args.get("wide").filter(|v| !v.is_null()) {
        None => false,
        Some(v) => v.as_bool().ok_or_else(|| bad("wide (true or false)"))?,
    };
    let reference = match (args.get("compare").filter(|v| !v.is_null()), args.get("reference").filter(|v| !v.is_null()))
    {
        (Some(_), Some(_)) => return Err(bad("compare and reference: give one")),
        (Some(p), None) => {
            let path = p.as_str().ok_or_else(|| bad("compare (a path)"))?;
            let bytes = read_file(path)?;
            Some(String::from_utf8(bytes).map_err(|_| ToolError::Failed(format!("{path}: not UTF-8 text")))?)
        }
        (None, Some(t)) => Some(String::from(t.as_str().ok_or_else(|| bad("reference (text)"))?)),
        (None, None) => None,
    };
    Ok(Args { wide, reference })
}

/// The answer over any register reader (pure: host-tested with the fake image): the snapshot text with its JSON
/// line, or the diff against the reference.
pub fn respond(
    regs: &impl crate::attach::Regs,
    args: &Args,
    head: &[String],
    function: &str,
    attach: &str,
) -> Result<String, ToolError> {
    let snap = snapshot::take(regs, args.wide);
    let topo = kms::read(regs, args.wide);
    let Some(text) = &args.reference else {
        let json = snapshot::to_json(&snap, function, attach);
        return Ok(snapshot_text(head, attach, &topo, &json));
    };
    let reference = snapshot::parse_reference(text).map_err(ToolError::Failed)?;
    let report = snapshot::compare(&reference, &snap.regs, args.wide);
    if !report.equal() {
        klog::event!(
            Info,
            "dcn",
            "snapshot.differs",
            differ = report.differ.len(),
            only_reference = report.only_reference.len(),
            only_now = report.only_now.len()
        );
    }
    let mut s = snapshot::render(&report);
    s += "attach: ";
    s += attach;
    s.push('\n');
    Ok(s)
}

fn dcn_snapshot(ctx: &ToolContext, args: &Json) -> ToolResult {
    let args = parse_args(args, |path| mcp::tools::files::read_whole(ctx, "dcn_snapshot", path, REFERENCE_MAX))?;
    let Some(g) = Gpu::get() else {
        return Err(ToolError::Failed(String::from(
            "no display engine (no AMD display function attached; QEMU has none)",
        )));
    };
    let regs: &DcnRegs = g.regs;
    let (before, _) = dcn::write_counts();
    let attach = attach_text(g);
    let answer = respond(regs, &args, &gpu::describe(g), g.function(), &attach)?;
    if dcn::write_counts().0 != before {
        // Not this tool (it never writes): another user wrote meanwhile; the log says so.
        klog::event!(Info, "dcn", "snapshot.concurrent_write", function = g.function());
    }
    Ok(vec![ToolOutput::Text(answer)])
}

mcp::mcp_tool!(
    name: "dcn_snapshot",
    owner: "GX",
    safety: Read,
    description: "Radeon display engine (DCN 3.1.4): reads the display registers V1's G-1 snapshot covers plus the pipe/OTG/DIG/DMCUB-window registers the attach gates and the KMS view use (reads only; gated pipes and switched-off DP encoders are not read). Default narrow: only OTG0, an OTG a powered pipe drives, DIG0/DP0; wide=true also reads OTG1-3, their DTOs, DIG1-4 and DP1-4. Answer: the device object (PCI, IP blocks), the attach gates' verdict (V1 gates 1-8), CRTC/plane/encoder/connector lines and one JSON line {format, function, attach, read, skipped, regs:{\"OFFSET\":\"VALUE\"}} to store as a reference. With compare (path of a stored answer's JSON line or of G-1 lines '0xOFFSET 0xVALUE') or reference (that text itself): the diff instead, first line 'equal' or 'differ'; status, pending, sticky and heartbeat bits are not compared; registers outside the reference's scope (G-1, narrow vs wide) are counted, not compared.",
    schema: r#"{"type":"object","properties":{"compare":{"type":"string","description":"absolute path of a stored reference (dcn_snapshot JSON or G-1 lines)"},"reference":{"type":"string","description":"the reference text itself"},"wide":{"type":"boolean","description":"also read OTG1-3, DTO1-3, DIG1-4, DP1-4 (default false)"}}}"#,
    handler: dcn_snapshot,
);

/// The write log as text (pure).
pub fn writes_text(total: u64, mismatched: u64, recent: &[dcn::LoggedWrite]) -> String {
    let mut s = format!("writes {total}, read back different {mismatched}, last {} shown\n", recent.len());
    for w in recent {
        s += &format!(
            "#{} {:05X} {} wrote {:#010x} read {:#010x}{}\n",
            w.seq,
            w.offset,
            snapshot::name(w.offset),
            w.wrote,
            w.read,
            match (w.read != w.wrote, w.masked) {
                (false, _) => "",
                (true, true) => " (status bits)",
                (true, false) => " MISMATCH",
            }
        );
    }
    s
}

fn dcn_writes(_ctx: &ToolContext, _args: &Json) -> ToolResult {
    let (total, mismatched) = dcn::write_counts();
    let mut s = writes_text(total, mismatched, &dcn::recent_writes());
    if DcnRegs::attached_now().is_none() {
        s += "no display engine attached\n";
    }
    Ok(vec![ToolOutput::Text(s)])
}

mcp::mcp_tool!(
    name: "dcn_writes",
    owner: "GX",
    safety: Read,
    description: "Radeon display engine: the DcnRegs write log since boot (every display register write is read back and logged): total, how many read back a different value, and the most recent writes with offset, register name, value written and value read back.",
    schema: r#"{"type":"object","properties":{}}"#,
    handler: dcn_writes,
);

/// `dcn_flip`'s arguments: (test, flips, copy).
pub fn flip_args(args: &Json) -> Result<(String, u64, bool), ToolError> {
    let test = match args.get("test") {
        None => "state",
        Some(v) => v.as_str().ok_or_else(|| ToolError::BadArgument(String::from("test must be a string")))?,
    };
    if !matches!(test, "state" | "pace" | "crc") {
        return Err(ToolError::BadArgument(format!("test {test:?}: one of state, pace, crc")));
    }
    let flips = match args.get("flips") {
        None => 180,
        Some(v) => v.as_u64().ok_or_else(|| ToolError::BadArgument(String::from("flips must be a whole number")))?,
    };
    let copy = match args.get("copy") {
        None => false,
        Some(v) => v.as_bool().ok_or_else(|| ToolError::BadArgument(String::from("copy must be true or false")))?,
    };
    Ok((String::from(test), flips, copy))
}

/// The longest `pace` run (it holds the screen; a mistyped count must not tie the screen up): 60 s of frames.
pub const PACE_SPAN_NS: u64 = 60_000_000_000;

/// `pace` with `flips` frames of `period_ns` fits in `PACE_SPAN_NS`.
pub fn pace_fits(flips: u64, period_ns: u64) -> Result<(), ToolError> {
    let period = period_ns.max(1);
    if flips.saturating_mul(period) > PACE_SPAN_NS {
        return Err(ToolError::BadArgument(format!("flips {flips}: at most {} at this rate (60 s)", PACE_SPAN_NS / period)));
    }
    Ok(())
}

fn frame_copy(base: &[u32], f: impl Fn(u32) -> u32) -> Result<alloc::vec::Vec<u32>, ToolError> {
    let mut v = alloc::vec::Vec::new();
    v.try_reserve_exact(base.len()).map_err(|_| ToolError::Failed(String::from("no memory for a frame")))?;
    v.extend(base.iter().map(|&p| f(p)));
    Ok(v)
}

fn dcn_flip(ctx: &ToolContext, args: &Json) -> ToolResult {
    let (test, flips, copy) = flip_args(args)?;
    let writes = |s: &mut String| {
        let (t, m) = dcn::write_counts();
        *s += &format!("display writes {t}, read back different {m}\n");
    };
    let s = crate::live::attach();
    let mut out = crate::live::state_line();
    out.push('\n');
    if test == "state" {
        writes(&mut out);
        return Ok(vec![ToolOutput::Text(out)]);
    }
    let Some(s) = s else {
        return Err(ToolError::Failed(out + "no page flips here (QEMU has no AMD display engine)"));
    };
    if !s.active() && !s.rearm() {
        return Err(ToolError::Failed(out + "the surface fell back to one image and A0 is not settled yet"));
    }
    if test == "pace" {
        pace_fits(flips, s.timing().period_ns())?;
    }
    let Some((w, h, base)) = display::snapshot() else {
        return Err(ToolError::Failed(String::from("no picture to start from")));
    };
    if (w, h) != s.size() || base.len() != (w as usize) * (h as usize) {
        return Err(ToolError::Failed(format!("console frame {w}x{h}, scanout {:?}", s.size())));
    }
    let guard = display::take_scanout("gx.flip")
        .map_err(|e| ToolError::Failed(format!("the screen is not free: {e:?}")))?;
    let mut src = frame_copy(&base, |p| p)?;
    let sleep = |ns: u64| sched::sleep_ns(ns);
    let line = match test.as_str() {
        "pace" => {
            let stop = || ctx.stop_requested();
            let p = crate::flip_test::pace(s, &time::now_ns, &sleep, &base, &mut src, flips, copy, &stop);
            p.text(copy, s.timing().period_ns())
        }
        _ => {
            let q = frame_copy(&base, |p| p ^ 0x00FF_FFFF)?;
            match crate::flip_test::crc(s, &sleep, &base, &q, 4) {
                Ok(r) => crate::flip_test::crc_text(&r),
                Err(e) => format!("crc: {e}"),
            }
        }
    };
    // The console's picture back in the buffer that stays shown, then the console has the screen again.
    match s.begin(&base, w) {
        Ok(b) => {
            let _ = b.present(&base, w, (0, 0, w, h));
            let _ = s.commit().and_then(|q| s.wait(q));
        }
        Err(_) => {
            let _ = s.present_shown(&base, w, (0, 0, w, h));
        }
    }
    drop(guard);
    out = line + "\n" + &crate::live::state_line() + "\n";
    writes(&mut out);
    Ok(vec![ToolOutput::Text(out)])
}

mcp::mcp_tool!(
    name: "dcn_flip",
    owner: "GX",
    safety: Change,
    description: "Radeon display engine page flips (GX-3): test=state (default) shows whether the display flips between A0 (GOP framebuffer) and A1, the flip counts and late flips. test=pace takes the screen for a moment and moves a white vertical bar over the console picture, one flip per frame (flips, default 180: about 3 s at 60 Hz; at most 60 s of frames, 3600 at 60 Hz); the answer gives the time, late flips and after how many vblanks each landed. copy=true copies the bar into the shown buffer instead (the old path: it tears; for the before/after look). test=crc switches the OTG CRC on, shows the inverted picture in A1 and the picture in A0 four times each and reads the CRC two frames after each flip (it must differ between the buffers and repeat per buffer), then puts the CRC registers back. The console picture is put back and the screen returned at the end. Writes display registers (surface address pair, update lock, CRC control).",
    schema: r#"{"type":"object","properties":{"test":{"type":"string","enum":["state","pace","crc"]},"flips":{"type":"integer","minimum":0},"copy":{"type":"boolean"}}}"#,
    handler: dcn_flip,
);

/// `display_mode`'s arguments: what to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModeArgs {
    State,
    Set(String),
    Confirm,
    Undo,
}

/// Parses `display_mode`'s arguments: none (state and list), `set` "WxH[@R]", `confirm` true, `undo` true; one
/// at a time.
pub fn mode_args(args: &Json) -> Result<ModeArgs, ToolError> {
    let bad = |s: &str| ToolError::BadArgument(String::from(s));
    let set = args.get("set").map(|v| v.as_str().map(String::from).ok_or_else(|| bad("set must be a string like \"1920x1080@60\"")));
    let flag = |k: &str| match args.get(k) {
        None => Ok(false),
        Some(v) => v.as_bool().ok_or_else(|| ToolError::BadArgument(format!("{k} must be true or false"))),
    };
    let (confirm, undo) = (flag("confirm")?, flag("undo")?);
    match (set.transpose()?, confirm, undo) {
        (None, false, false) => Ok(ModeArgs::State),
        (Some(m), false, false) => Ok(ModeArgs::Set(m)),
        (None, true, false) => Ok(ModeArgs::Confirm),
        (None, false, true) => Ok(ModeArgs::Undo),
        _ => Err(bad("one of set, confirm, undo at a time")),
    }
}

fn display_mode(_ctx: &ToolContext, args: &Json) -> ToolResult {
    let a = mode_args(args)?;
    let r = match a {
        ModeArgs::State => crate::modeset::text(),
        ModeArgs::Set(m) => crate::modeset::set(&m).inspect(|line| {
            if line.contains("keep it") {
                console::console().write_str(&format!("\n{line}\n"), console::FG);
            }
        }),
        ModeArgs::Confirm => crate::modeset::confirm(),
        ModeArgs::Undo => crate::modeset::undo(),
    };
    match r {
        Ok(t) => Ok(vec![ToolOutput::Text(t + "\n")]),
        Err(e) => Err(ToolError::Failed(e)),
    }
}

mcp::mcp_tool!(
    name: "display_mode",
    owner: "GX",
    safety: Change,
    description: "Radeon display engine mode switch (GX-4): no arguments = the link's timing (kept as the firmware lit it), the size shown, the confirmed size, a confirmation still waiting, and the modes the monitor offers (EDID, else the GOP's list). set=\"WxH\" or \"WxH@R\" (e.g. \"1920x1080@60\") shows that size scaled by the display engine to the link's timing (only the link's rate; only sizes up to the link's); the console and /dev/fb0 follow the new size. A new size goes back by itself after 15 s unless confirm=true comes first (or `mode keep` on the console); undo=true goes back at once. A size that does not come up (update not taking effect, read-back, OTG stopped, underflow) goes back at once. Writes display registers (viewport, scaler mode, update lock).",
    schema: r#"{"type":"object","properties":{"set":{"type":"string"},"confirm":{"type":"boolean"},"undo":{"type":"boolean"}}}"#,
    handler: display_mode,
);
