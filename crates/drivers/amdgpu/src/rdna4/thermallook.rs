//! Wave 44: what the thermal taker shows: its state in plain words (the stage `state`, the `gpu` tool) and one
//! look, the card's limits and one fresh read of its temperatures (the `gpu` tool, the measurement run).

use super::thermal::Thermal;
use crate::gpucore::psp::Hw;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rdna4_core::metrics14::Temps;
use rdna4_core::smu14::CTF_OFFSET;
use rdna4_core::thm14;

impl Thermal {
    /// The taker's state in plain words (the stage `state` and the `gpu` tool).
    pub fn describe(&self) -> String {
        let alert = match (self.alert_armed, self.range) {
            (true, Some(r)) => format!("armed, upper threshold {} C (the table's software shutdown temperature; offset {} C for a fan fault)", self.high_now.unwrap_or(thm14::high_fan_recovery(r.software_shutdown_temp)), r.software_shutdown_temp_offset),
            (true, None) => String::from("armed, limits unknown"),
            (false, _) => String::from("not armed in this boot"),
        };
        let ctx: Vec<String> = self.contexts.iter().map(|(c, n)| format!("{} {n}", thm14::context_name(*c))).collect();
        let foreign: Vec<String> = self.foreign.iter().map(|((c, s), n)| format!("client {c} source {s}: {n}")).collect();
        let list = |v: Vec<String>| if v.is_empty() { String::from("none") } else { v.join(", ") };
        format!(
            "thermal alert {alert}; taken: rose above {}, fell below {}, power controller {} ({}); other thermal sources {}; check {}; {} checks ({} late); temperatures {}; reads {}{}; waiting acknowledges {} ({} lost); {} refused or failed accesses; {} cancelled by a PMFW reload; last result: {}{}",
            self.count[0],
            self.count[1],
            self.count[2],
            list(ctx),
            list(foreign),
            self.due_us.map_or(String::from("none pending"), |d| format!("pending (due at {d} us)")),
            self.checks,
            self.late,
            self.last.map_or(String::from("not read"), |(t, _)| format!("edge {} C, hotspot {} C, memory {} C", t.edge, t.hotspot, t.mem)),
            self.reads,
            self.read_failed.as_ref().map_or(String::new(), |e| format!(" (the last failed: {e})")),
            self.queue.len(),
            self.lost,
            self.failed,
            self.cancelled,
            self.outcome.as_deref().unwrap_or("none"),
            self.off_failed.as_ref().map_or(String::new(), |e| format!("; THE POWER-OFF DID NOT HAPPEN: {e}"))
        )
    }

    /// The limits and one fresh read, for the read-only `gpu` tool and the measurement run (one transfer).
    pub fn look(&mut self, hw: &mut dyn Hw, now_us: u64) -> Look {
        let temps = self.read(hw, now_us);
        let mut lines = self.limit_lines();
        lines.push(match &temps {
            Ok(t) => format!(
                "temperatures (one metrics transfer, the PMFW's moving averages): edge {} C, hotspot {} C, memory {} C, counter {}{}",
                t.edge,
                t.hotspot,
                t.mem,
                t.counter,
                if t.all_zero() { "; all three 0: a table nothing wrote, most likely" } else { "" }
            ),
            Err(e) => format!("temperatures: not read ({e})"),
        });
        lines.push(format!("thermal taker: {}", self.describe()));
        Look { lines, temps, limits: self.limits() }
    }

    /// The limits only, with nothing sent (what is known without the card).
    pub fn limit_lines(&self) -> Vec<String> {
        alloc::vec![match self.range {
            Some(r) => format!(
                "thermal limits (the card's table): edge {} C, hotspot {} C, memory {} C (emergency +{CTF_OFFSET} C each), software shutdown {} C, fan fault offset {} C",
                r.max / 1000,
                r.hotspot_crit_max / 1000,
                r.mem_crit_max / 1000,
                r.software_shutdown_temp,
                r.software_shutdown_temp_offset
            ),
            None => String::from("thermal limits: not known (the power controller's start did not read its table in this boot)"),
        }]
    }
}

/// One look: its lines, the temperatures read, the limits (edge, hotspot, memory, software shutdown).
pub struct Look {
    pub lines: Vec<String>,
    pub temps: Result<Temps, String>,
    pub limits: Option<(i32, i32, i32, u16)>,
}

impl Look {
    /// A look with nothing read, and why.
    pub fn none(lines: Vec<String>, why: &str) -> Look {
        Look { lines, temps: Err(String::from(why)), limits: None }
    }
}

