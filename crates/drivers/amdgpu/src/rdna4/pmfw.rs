//! Wave 43 (piece 9 beyond read-only): the power controller of one card in this boot. Its firmware (the PMFW) is
//! started by the secure processor when the stage `fw` loads it; the stage `pmfw` then makes Linux's start of it
//! (`stage_pmfw`), once per start of that firmware. What that start learned stays here for the stages after it:
//! whether the graphics block has power, the clocks' levels (the display needs them), the PMFW's table, the
//! thermal range, and the two tables in VRAM the PMFW was told about.

use super::smu14::Line;
use super::vram::{Bump, Layout};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use core::ops::Range;
use rdna4_core::smu14::{DRIVER_TABLE_BYTES, TOOLS_TABLE_BYTES};
use rdna4_core::smuseq::{Facts, Script, Stopped, Tables};

/// The names the two tables are taken under from the card's VRAM manager.
pub const TOOLS_TABLE: &str = "the PMFW's tools table";
pub const DRIVER_TABLE: &str = "the PMFW's driver table";

/// Where the VRAM manager's own record has the two tables, as MC addresses: what the comparison with Linux's
/// trace holds the stage's arguments against (`stage_pmfw::linux_rules`). Read from the manager by name and
/// size, not from what the stage kept, so a stage that names another place than it took differs.
pub fn placed(l: &Layout, b: &Bump) -> Option<Tables> {
    let at = |what, bytes| b.taken.iter().find(|(w, r)| *w == what && r.end - r.start == bytes).map(|(_, r)| l.mc(r.start));
    Some(Tables { driver_mc: at(DRIVER_TABLE, DRIVER_TABLE_BYTES)?, tools_mc: at(TOOLS_TABLE, TOOLS_TABLE_BYTES)? })
}

#[derive(Debug)]
pub struct State {
    /// The tools table and the driver table (VRAM offsets), taken from the card's VRAM manager at the first
    /// `pmfw` of a boot and never given back: the PMFW keeps both addresses for as long as it runs.
    pub tables: Option<(Range<u64>, Range<u64>)>,
    /// A `pmfw` call sent the PMFW that runs now a message: the start is not made a second time on it.
    pub began: bool,
    /// Linux's `smu_hw_init` came to its end on the PMFW that runs now.
    pub passed: bool,
    pub stopped: Option<Stopped>,
    /// What the start learned (also of a start that stopped, as far as it came).
    pub facts: Facts,
    /// A message of the look `smu` went out and its answer was not seen: Linux's first message of the start does
    /// not look at the answer register, so the stage waits for nothing and refuses instead.
    pub owes_answer: bool,
    /// The stage's own sequence and mailbox while a call runs: after a stop, what the stop leaves to do. On the
    /// heap: the session is moved about by value.
    pub(super) run: Option<Box<(Script, Line)>>,
    /// The stage `fw` started the graphics block anew for a load of the firmware (`gcstage::before_fw`) and the
    /// PMFW's own image was not answered yet. The PMFW before may still run and keeps what it was told, but its
    /// start no longer carries the chain (`Session::passed`): `gcboot` waits for the start on the new one.
    pub reload_began: bool,
}

impl State {
    pub const fn new() -> State {
        State { tables: None, began: false, passed: false, stopped: None, facts: Facts::new(), owes_answer: false, run: None, reload_began: false }
    }

    /// EnableAllSmuFeatures was answered OK on the PMFW that runs now. Linux starts the graphics block, GFXHUB
    /// and SDMA only after that (the trace's order of sections: smu, then gc-boot, gc-hub, sdma), and takes the
    /// block as powered from there on. That it has no power before is not known: Linux itself reads and writes
    /// GC:RLC_SPM_MC_CNTL long before section smu (init.txt lines 316-317; plan/spec/gpu-rdna4.md section 7).
    pub fn gc_powered(&self) -> bool {
        self.facts.enabled
    }

    /// The secure processor loaded the PMFW's firmware again (the stage `fw`): what the PMFW before it was told
    /// and answered is gone. The two tables stay taken (the same places are named again).
    pub fn reloaded(&mut self) {
        *self = State { tables: self.tables.take(), ..State::new() };
    }

    pub fn describe(&self) -> String {
        let how = match (self.passed, self.began, self.stopped) {
            (true, _, _) if self.reload_began => String::from("started, but the stage fw began to load its firmware anew and that load was not answered: its start is due again after fw"),
            (true, _, _) => String::from("started"),
            (false, true, Some(why)) => format!("stopped half-way ({why:?}); not started again until the stage fw reloads its firmware"),
            (false, true, None) => String::from("stopped half-way"),
            (false, false, _) => String::from("not started"),
        };
        format!(
            "{how}, graphics block {}, running features {}, {} clock tables",
            if self.gc_powered() { "powered" } else { "not powered by this boot" },
            self.facts.features.map_or(String::from("not read"), |m| format!("{m:#018x}")),
            self.facts.dpm.len()
        )
    }
}

impl Default for State {
    fn default() -> Self {
        State::new()
    }
}
