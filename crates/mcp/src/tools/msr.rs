//! `msr_read`: one model-specific register of the CPU this runs on (another CPU needs a run-on-CPU primitive,
//! change request to CP/SC; until CP-4 starts the other cores there is only one), only through `hal::msr::read_checked`,
//! which refuses every MSR CPUID (or the machine-check bank's STATUS) does not vouch for: a read of an
//! unimplemented MSR would fault, and the request comes from the network.

use super::{bad, number};
use crate::{Json, ToolContext, ToolError, ToolOutput, ToolResult, mcp_tool};
use alloc::format;

fn msr_read(_ctx: &ToolContext, args: &Json) -> ToolResult {
    let msr = number(args, "msr")?.ok_or_else(|| bad("msr"))?;
    let Ok(msr) = u32::try_from(msr) else { return Err(bad("msr")) };
    // Interrupts off between the CPU number and the read: no preemption, so the label names the CPU read.
    let (cpu, value) = {
        let _irq = hal::cpu::IrqGuard::new();
        (hal::percpu::this_cpu(), hal::msr::read_checked(msr))
    };
    match value {
        Some(v) => Ok(alloc::vec![ToolOutput::Text(format!("msr {msr:#x} = {v:#018x} (cpu {cpu}, the CPU this call ran on)"))]),
        None => {
            klog::event!(Info, "mcp", "msr.refused", msr = klog::Hex(msr as u64));
            Err(ToolError::Refused(format!(
                "MSR {msr:#x} is not on the read list (APIC base, PAT, EFER, STAR/LSTAR/FMASK, and MTRR, TSC_AUX and machine-check registers when CPUID says they exist)"
            )))
        }
    }
}

mcp_tool!(
    name: "msr_read",
    owner: "MC",
    safety: Read,
    description: "Reads one model-specific register of the CPU the call runs on (named in the answer). Only MSRs the CPU is known to have are read (hal's read list: APIC base, PAT, EFER, STAR/LSTAR/FMASK, MTRRCAP/MTRR_DEF_TYPE, TSC_AUX, MCG_CAP/STATUS and the machine-check banks); others are refused instead of faulting.",
    schema: r#"{"type":"object","properties":{"msr":{"type":["integer","string"],"description":"MSR number, e.g. 3221225600 or \"0xC0000080\""}},"required":["msr"]}"#,
    handler: msr_read,
);
