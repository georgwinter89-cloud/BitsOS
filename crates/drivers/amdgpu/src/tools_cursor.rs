//! GX-5a: `dcn_cursor` (safety Change: it writes the cursor registers), the desk's check of the hardware cursor
//! without a compositor: a built-in arrow shown, moved, swept along the edges (partly off the screen), hidden.

use crate::attach::Regs;
use crate::cursor::{Alpha, CursorError, Shape};
use crate::layout as l;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult};

/// What `dcn_cursor` is asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CursorArgs {
    State,
    Show { size: u32, x: i32, y: i32 },
    Move { x: i32, y: i32 },
    Sweep { frames: u32 },
    Hide,
}

/// Longest sweep: 60 s at 60 Hz.
pub const SWEEP_MAX: u32 = 3600;

pub fn cursor_args(args: &Json) -> Result<CursorArgs, ToolError> {
    let bad = |s: &str| ToolError::BadArgument(String::from(s));
    let int = |k: &str, d: i64| match args.get(k) {
        None => Ok(d),
        Some(v) => v.as_i64().ok_or_else(|| ToolError::BadArgument(format!("{k} must be an integer"))),
    };
    let coord = |k: &str| int(k, 0).and_then(|v| i32::try_from(v).map_err(|_| bad("x and y must fit 32 bits")));
    let op = match args.get("op") {
        None => "state",
        Some(v) => v.as_str().ok_or_else(|| bad("op must be a string"))?,
    };
    match op {
        "state" => Ok(CursorArgs::State),
        "show" => {
            let size = int("size", 64)?;
            if ![32, 48, 64, 96, 128, 256].contains(&size) {
                return Err(bad("size must be 32, 48, 64, 96, 128 or 256"));
            }
            Ok(CursorArgs::Show { size: size as u32, x: coord("x")?, y: coord("y")? })
        }
        "move" => Ok(CursorArgs::Move { x: coord("x")?, y: coord("y")? }),
        "sweep" => {
            let f = int("frames", 240)?;
            if !(1..=i64::from(SWEEP_MAX)).contains(&f) {
                return Err(bad("frames must be 1 to 3600"));
            }
            Ok(CursorArgs::Sweep { frames: f as u32 })
        }
        "hide" => Ok(CursorArgs::Hide),
        _ => Err(bad("op is one of state, show, move, sweep, hide")),
    }
}

/// The test arrow: `size` x `size`, hotspot (0, 0), white with a black edge, pre-multiplied ARGB8888.
pub fn arrow(size: u32) -> Vec<u32> {
    let h = size * 3 / 4;
    let inside = |x: i64, y: i64| x >= 0 && y >= 0 && y < i64::from(h) && x * 5 <= y * 3;
    let edge = (size / 32).max(1) as i64;
    let mut px = vec![0u32; (size * size) as usize];
    for y in 0..size as i64 {
        for x in 0..size as i64 {
            if !inside(x, y) {
                continue;
            }
            let rim = (-edge..=edge).any(|d| !inside(x + d, y) || !inside(x, y + d));
            px[(y as u64 * u64::from(size) + x as u64) as usize] = if rim { 0xFF00_0000 } else { 0xFFFF_FFFF };
        }
    }
    px
}

/// The sweep's places on a `w` x `h` screen for an arrow of `size`: the four corners, then each edge with the
/// arrow half off the screen, then fully off (hidden by the hardware), then the middle.
pub fn sweep_marks(w: u32, h: u32, size: u32) -> [(i32, i32); 10] {
    let (w, h, s) = (w as i32, h as i32, size as i32);
    [(0, 0), (w - 1, 0), (w - 1, h - 1), (0, h - 1), (-s / 2, h / 2), (w - s / 2, h / 2), (w / 2, -s / 2), (w / 2, h - s / 2), (-s, -s), (w / 2, h / 2)]
}

fn err_text(e: CursorError) -> String {
    match e {
        CursorError::NoDevice(w) => format!("no hardware cursor: {w}"),
        CursorError::Invalid(w) => format!("refused: {w}"),
        CursorError::Busy => String::from("another cursor image is being written"),
        CursorError::Reg(r) => format!("register: {r:?}"),
    }
}

fn state_text(s: &crate::live::KScanout) -> String {
    let (shape, tip, shown, regs) = s.cursor_state();
    let p = s.pipes().first().map_or(0, |p| u32::from(p.hubp));
    let rd = |r: &crate::live::Live, b: u32| r.read(b).map_or(String::from("?"), |v| format!("{v:#010x}"));
    let hw = s.with_pair(|r| {
        format!(
            "CURSOR_CONTROL {} SURFACE {}:{} SIZE {} POSITION {} HOT_SPOT {} DST_OFFSET {} CNVC_CUR0_CONTROL {} MPCC_OPP_ID {}",
            rd(r, l::hubp(p, l::CUR_CONTROL)),
            rd(r, l::hubp(p, l::CUR_ADDR_HI)),
            rd(r, l::hubp(p, l::CUR_ADDR)),
            rd(r, l::hubp(p, l::CUR_SIZE)),
            rd(r, l::hubp(p, l::CUR_POSITION)),
            rd(r, l::hubp(p, l::CUR_HOT_SPOT)),
            rd(r, l::hubp(p, l::CUR_DST_OFFSET)),
            rd(r, l::dpp(p, l::CNVC_CUR_CONTROL)),
            rd(r, l::mpcc(p, l::MPCC_OPP_ID)),
        )
    });
    format!("cursor: image {shape:?}, pointer {tip:?}, shown {shown}, registers {regs:?}\n{hw}\n")
}

fn dcn_cursor(_ctx: &ToolContext, args: &Json) -> ToolResult {
    let a = cursor_args(args)?;
    let Some(s) = crate::live::attach() else {
        return Err(ToolError::Failed(String::from("no hardware cursor here (no AMD display engine taken over: QEMU)")));
    };
    let fail = |e: CursorError| ToolError::Failed(err_text(e) + "\n" + &state_text(s));
    let mut out = String::new();
    match a {
        CursorArgs::State => {}
        CursorArgs::Show { size, x, y } => {
            let shape = Shape::new(size, size, 0, 0, Alpha::Premultiplied).map_err(fail)?;
            s.cursor_image(shape, &arrow(size), (x, y)).map_err(fail)?;
        }
        CursorArgs::Move { x, y } => s.cursor_move((x, y)).map_err(fail)?,
        CursorArgs::Hide => s.cursor_hide().map_err(fail)?,
        CursorArgs::Sweep { frames } => {
            let (w, h) = s.size();
            let size = s.cursor_state().0.map_or(64, |sh| sh.pitch());
            let period = s.timing().period_ns().max(1);
            let marks = sweep_marks(w, h, size);
            let (mut worst, mut sum) = (0u64, 0u64);
            for f in 0..frames {
                // A loop around the screen, one place per frame, the marks held at the quarter points.
                let m = marks[(f as usize * marks.len()) / frames as usize];
                let t0 = time::now_ns();
                s.cursor_move(m).map_err(fail)?;
                let ns = time::now_ns().saturating_sub(t0);
                worst = worst.max(ns);
                sum += ns;
                sched::sleep_ns(period);
            }
            out += &format!("sweep: {frames} moves over {} places, move cost avg {} ns, worst {} ns\n", marks.len(), sum / u64::from(frames), worst);
        }
    }
    let (t, m) = crate::dcn::write_counts();
    out += &state_text(s);
    out += &format!("display writes {t}, read back different {m}\n");
    Ok(vec![ToolOutput::Text(out)])
}

mcp::mcp_tool!(
    name: "dcn_cursor",
    owner: "GX",
    safety: Change,
    description: "Radeon display engine hardware cursor (GX-5a): op=state (default) shows the cursor's state and its registers (CURSOR_CONTROL, surface, size, position, hotspot, CNVC_CUR0_CONTROL, the MPC tree's OPP). op=show size=32|48|64|96|128|256 (default 64) x y shows a white arrow with a black edge, its tip on x,y (screen pixels of the mode shown; may be negative or past the edge: the part on the screen shows). op=move x y moves it (shows it again after a hide). op=sweep frames (default 240, at most 3600) moves it once per frame around corners and edges, half off the screen, fully off and back to the middle, and answers the time one move takes. op=hide hides it. Writes display registers (cursor surface, position, enables, cursor lock).",
    schema: r#"{"type":"object","properties":{"op":{"type":"string","enum":["state","show","move","sweep","hide"]},"size":{"type":"integer"},"x":{"type":"integer"},"y":{"type":"integer"},"frames":{"type":"integer","minimum":1}}}"#,
    handler: dcn_cursor,
);
