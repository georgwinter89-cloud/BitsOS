//! The console command `hang` (IN-3 change request 8, applied by INT-A1): Georg's A1 watchdog test from the
//! keyboard. `physical_only`: a line with any injected (MCP) key is refused by the monitor before this runs; the
//! MCP side has its own `hang` tool with `confirm`. Refused unless the watchdog is armed (otherwise nobody
//! would reset the machine).

use crate::system::{self, HangMode};
use crate::tool::hang_refusal;
use alloc::format;
use alloc::string::String;

fn hang(c: &mut console::monitor::Ctx) -> Result<(), String> {
    let Some(mode) = HangMode::parse(c.args.first().copied().unwrap_or("")) else {
        return Err(String::from("usage: hang [all|one|halt]"));
    };
    let st = system::state();
    if let Some(why) = hang_refusal(st) {
        klog::event!(Warn, "watchdog", "hang.refused", source = "console", state = st.name());
        return Err(format!("refused: {why}"));
    }
    let timeout = system::config().map_or(crate::config::DEFAULT_TIMEOUT_S, |c| c.timeout_s);
    let what = match mode {
        HangMode::All => "every CPU now spins",
        HangMode::One => "this CPU now spins (the others run on)",
        HangMode::Halt => "every CPU now halts",
    };
    c.println(&format!(
        "Hanging on purpose: {what} with interrupts off. The watchdog resets the machine about {timeout} s after its last kick; BitsOS then starts again by itself."
    ));
    klog::event!(Warn, "watchdog", "hang.requested", source = "console", mode = mode.name(), timeout_s = timeout; "typed on the console's keyboard");
    system::deliberate_hang(mode)
}
console::command!(name: "hang", args: "[all|one|halt]", help: "hangs the machine on purpose: the armed watchdog must reset it (keyboard only)", physical_only: true, run: hang);
