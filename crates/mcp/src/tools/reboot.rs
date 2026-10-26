//! `reboot`: answer first, then restart (V1 `mcp_reboot` = 2, `chk-restart.mjs`). The answer names the
//! generation that runs and the one bitsboot will start next (`KERNEL` or the fallback `KERNEL.OLD`), so the
//! desk knows whether a Reload is needed afterwards; Georg's boot order brings the machine back to V2 while the
//! stick is in. Then every driver is quiesced (CONTRACTS 6a) and CP-5's platform reset runs
//! (`acpi::power::reboot`, `spec/core.md` §4): the FADT reset register (Mini-PC: I/O 0xB2 <- 0xBE), 0xCF9 (2,
//! then 6; then 2, then 0xE for a full reset), the keyboard controller where the FADT does not rule one out,
//! 50 ms each, and the triple fault last, which resets every PC. So a reboot always ends the run. A machine
//! whose firmware declares no reset path at all (no FADT reset register, no 8042 named) is unusual enough that
//! a real reboot there still needs `force` (MC-2's rule, kept): the run then ends by 0xCF9 or the triple fault.
//!
//! There is no power-off tool, on purpose (R8f): `acpi::power::poweroff` is for the console command and QEMU.
//!
//! MC-4b (the remote install chain): with the boot stick mounted, "next start" is what bitsboot's own rule
//! (`bitsboot_pure::gens::choose`) makes of the stick as it is now (`boot::next_start_now`), so after an
//! `install_kernel` the answer names the new generation's one try; without a stick it falls back to the guess
//! from this start's GENERATION record.
//! Before the reset every file and file system is written back (`vfs::writeback::sync_all`, FS-7 change
//! request 1): a `write_file` or an install's last stick write is on the stick before the machine resets. The
//! write-back runs in a helper thread with a deadline (B4, MC-4b fixer): a stick that hangs does not stop the
//! reset (`reboot.sync_timeout`).

use super::opt_bool;
use crate::boot;
use crate::bounded::{self, Bounded};
use crate::{Json, ToolContext, ToolError, ToolOutput, ToolResult, mcp_tool};
use acpi::power::{self, RebootPlan, Step};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// The reset paths (re-exported by `tools/mod.rs` under this name since MC-2; CP-5 moved them to `acpi::power`).
pub type ResetPlan = RebootPlan;

/// How long the write-back before the reset may take (B4): a hung or pulled stick must not keep the machine
/// from restarting after the client was told it would. Generous: a 120 MB install is on the stick already (the
/// install writes through and reads back), so what is left is the page cache's dirty rest.
const SYNC_DEADLINE_NS: u64 = 10_000_000_000;

/// After the answer: write back every file and file system (the stick's last writes, at most
/// `SYNC_DEADLINE_NS`), park the other CPUs, quiesce every driver, then the reset paths. Never returns.
fn reset(p: RebootPlan) {
    // RB-8: the journal writes down what is still in the ring before the reset (at most DRAIN_NS).
    let _ = klog::journal::wait_drained("mcp-reboot", klog::journal::DRAIN_NS, &sched::sleep_ns);
    match bounded::run("mcp-reboot-sync", SYNC_DEADLINE_NS, vfs::writeback::sync_all) {
        Bounded::Done(synced) if synced.failed > 0 => {
            klog::event!(Warn, "mcp", "reboot.synced", flushed = synced.flushed, failed = synced.failed;
                "some files or file systems could not be written back before the reset");
        }
        Bounded::Done(synced) => klog::event!(Info, "mcp", "reboot.synced", flushed = synced.flushed, failed = synced.failed),
        Bounded::TimedOut => klog::event!(Warn, "mcp", "reboot.sync_timeout", deadline_ms = SYNC_DEADLINE_NS / 1_000_000;
            "the write-back did not finish (a hung or pulled stick?): resetting anyway"),
        Bounded::NoThread | Bounded::Busy => klog::event!(Warn, "mcp", "reboot.sync_skipped";
            "no thread for the write-back (no memory for its stack): resetting without it"),
    }
    // RB-quiesce: the other CPUs parked first (on CPU 0); never released, the reset follows.
    let _park = platform::power::park_others("mcp-reboot");
    let devices = devmodel::quiesce_all();
    klog::event!(Info, "mcp", "reboot.quiesced", devices = devices.join(","));
    power::reboot(&p, &|ns| sched::sleep_ns(ns))
}

/// The answer's text (pure; host-tested).
fn answer(dry: bool, running: &str, next: &str, p: &RebootPlan) -> String {
    let steps: Vec<String> = p.steps.iter().map(Step::describe).collect();
    let mut t = format!(
        "{}: running {running}.\nnext start: {next}\nreset by: {}\nGeorg's boot order starts the stick again (V2) while it is plugged in.",
        if dry { "reboot (dry run, nothing happens)" } else { "rebooting once this answer is out" },
        steps.join(", then "),
    );
    if !p.skipped.is_empty() {
        t.push_str(&format!("\nnot offered by this machine: {}", p.skipped.join("; ")));
    }
    t
}

fn reboot(ctx: &ToolContext, args: &Json) -> ToolResult {
    let dry = opt_bool(args, "dry_run")?.unwrap_or(false);
    let force = opt_bool(args, "force")?.unwrap_or(false);
    let p = power::live_reboot_plan();
    let generation = boot::facts().and_then(|f| f.generation);
    let running = match generation {
        Some(g) => format!(
            "generation {} ({}{})",
            g.number,
            boot::running_reason(&g),
            boot::kernel_id_note(boot::facts().and_then(|f| f.reload))
        ),
        None => String::from("an unknown generation"),
    };
    let mut t = answer(dry, &running, &boot::next_start_now(generation), &p);
    if !p.declared() && !dry {
        if !force {
            klog::event!(Warn, "mcp", "reboot.undeclared");
            t.push_str("\nRefused: the firmware declares none of these paths (no FADT reset register, no 8042 named); force:true tries them anyway.");
            return Err(ToolError::Refused(t));
        }
        klog::event!(Warn, "mcp", "reboot.forced"; "no path the firmware declares: 0xCF9, the 8042 guess, then the triple fault");
    }
    if dry {
        return Ok(alloc::vec![ToolOutput::Text(t)]);
    }
    if !ctx.after_answer(Box::new(move || reset(p))) {
        return Err(ToolError::Failed(String::from("reboot happens only after its answer, and this call has no answer to wait for (called outside the MCP server, or past its deadline)")));
    }
    klog::event!(Info, "mcp", "reboot.requested", generation = generation.map(|g| g.number as i64).unwrap_or(-1));
    Ok(alloc::vec![ToolOutput::Text(t)])
}

mcp_tool!(
    name: "reboot",
    owner: "MC",
    safety: Change,
    description: "Restarts the machine after answering (every file and the boot stick written back first). The answer names the running system generation and the one the next start brings, as bitsboot's rule reads the boot stick now (KERNEL: the newest generation, e.g. one install_kernel just wrote, as its one try; KERNEL.OLD: the fallback; PINNED: SYS\\PIN; a stick that does not answer within 2 s: a guess from this start, said so); Georg's boot order brings it back to V2 while the stick is in. Prefer reload (keeps the newest kernel, takes seconds). dry_run answers without restarting. There is no power-off tool.",
    schema: r#"{"type":"object","properties":{"dry_run":{"type":"boolean","description":"only say what would happen"},"force":{"type":"boolean","description":"restart even when the firmware declares no reset path (0xCF9, the keyboard controller guess, then the triple fault)"}}}"#,
    handler: reboot,
);

#[cfg(test)]
mod tests {
    use super::*;
    use acpi::fadt::{Fadt, ResetReg, boot_arch};
    use acpi::sdt::{Gas, Space};

    fn minipc() -> Fadt {
        acpi::fadt::parse(&acpi::testkit::fadt_minipc(0x1000), 0x2000, &mut acpi::Notes::new()).expect("Mini-PC FADT")
    }

    #[test]
    fn minipc_answer_names_smm_cf9_and_the_triple_fault() {
        let p = power::reboot_plan(Some(&minipc()));
        let t = answer(false, "generation 3 (new)", "KERNEL", &p);
        assert!(t.contains("FADT reset register I/O 0xb2 <- 0xbe, then 0xcf9 <- 2, then 6, then 0xcf9 <- 2, then 0xe (full reset), then triple fault"), "{t}");
        assert!(t.contains("keyboard controller: the FADT says there is none"), "{t}");
        assert!(t.starts_with("rebooting once this answer is out"));
    }

    #[test]
    fn q35_answer_and_dry_run() {
        let mut f = minipc();
        f.reset = Some(ResetReg { reg: Gas { space: Space::Io, bit_width: 8, bit_offset: 0, access_size: 1, address: 0xCF9 }, value: 6 });
        f.boot_arch |= boot_arch::I8042;
        let p = power::reboot_plan(Some(&f));
        let t = answer(true, "an unknown generation", "KERNEL", &p);
        assert!(t.starts_with("reboot (dry run, nothing happens)"));
        assert!(t.contains("FADT reset register I/O 0xcf9 <- 0x6"), "{t}");
        assert!(t.contains("keyboard controller 0x64 <- 0xfe"), "{t}");
        assert!(!t.contains("not offered"), "{t}");
    }

    use crate::boot::next_from_listing;
    use bitsboot_pure::gens::GenDir;

    fn g(number: u32, markers: &str) -> GenDir {
        GenDir { number, kernel: true, initrd: true, new: markers.contains('N'), tried: markers.contains('T'), ok: markers.contains('O') }
    }

    #[test]
    fn next_start_follows_the_stick() {
        // After install_kernel: the new generation gets its one try.
        let t = next_from_listing(&[g(1, "O"), g(2, "N")], None);
        assert!(t.starts_with("KERNEL: generation 2 (NEWEST_TRY: newest, new"), "{t}");
        // Its one try was used and never confirmed: the fallback.
        let t = next_from_listing(&[g(1, "O"), g(2, "NT")], None);
        assert!(t.starts_with("KERNEL.OLD: generation 1 (FALLBACK_AFTER_TRY"), "{t}");
        let t = next_from_listing(&[g(1, "O"), g(2, "O")], None);
        assert!(t.starts_with("KERNEL: generation 2 (NEWEST_OK"), "{t}");
        let t = next_from_listing(&[g(1, "O"), g(2, "N")], Some(1));
        assert!(t.starts_with("PINNED: generation 1 (PINNED"), "{t}");
        // A cut-short install (no INITRD) falls back at once.
        let t = next_from_listing(&[g(1, "O"), GenDir { initrd: false, ..g(2, "") }], None);
        assert!(t.starts_with("KERNEL.OLD: generation 1 (FALLBACK_BAD_FILES"), "{t}");
        assert!(next_from_listing(&[], None).starts_with("nothing: bitsboot would refuse"));
    }

    #[test]
    fn without_a_fadt_the_triple_fault_still_resets() {
        let p = power::reboot_plan(None);
        assert_eq!(p.steps.last(), Some(&Step::TripleFault));
        assert!(answer(false, "x", "y", &p).contains("no FADT"));
    }
}
