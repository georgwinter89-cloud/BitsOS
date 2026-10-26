//! `hw_info`: the machine as the kernel sees it, in sections (V1's `HwInfo`, `spec/net.md` §5.2): kernel
//! (version, generation, Reload counter from BootInfo), cpu (CPUID facts, control registers, machine-check
//! banks), memory (frames, heap, firmware memory map), firmware (ACPI tables), interrupts (APIC, I/O APICs),
//! msi (PC-3: MSI/MSI-X sets with per-vector counts), pci (the device tree). Read-only: nothing here writes a
//! register.

use super::{bad, opt_str, text};
use crate::{Json, ToolContext, ToolResult, mcp_tool};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

const SECTIONS: &[&str] = &["kernel", "cpu", "memory", "firmware", "interrupts", "msi", "pci"];

fn cpu() -> Vec<String> {
    let f = hal::cpu::facts();
    let mut out = Vec::new();
    out.push(format!("cpu vendor={} brand=\"{}\" family={:#x} model={:#x} stepping={}", f.vendor, f.brand.trim(), f.family, f.model, f.stepping));
    out.push(format!(
        "cpu phys_bits={} virt_bits={} apic_id={} max_leaf={:#x} max_ext_leaf={:#x}",
        f.phys_addr_bits, f.virt_addr_bits, f.initial_apic_id, f.max_basic_leaf, f.max_ext_leaf
    ));
    let on: Vec<&str> = f.flags.iter().filter(|(_, v)| *v).map(|(n, _)| *n).collect();
    out.push(format!("cpu flags: {}", on.join(" ")));
    if !f.missing_v3.is_empty() {
        out.push(format!("cpu missing x86-64-v3: {}", f.missing_v3.join(" ")));
    }
    out.push(format!(
        "cpu cr0={:#x} cr4={:#x} efer={:#x} pat={:#018x} xcr0={:#x} xcr0_supported={:#x} xsave={} xsave_max={} xsaves={}",
        f.cr0, f.cr4, f.efer, f.pat, f.xcr0, f.xcr0_supported, f.xsave_size, f.xsave_size_max, f.xsaves_size
    ));
    out.push(format!("cpus online={} (hal::percpu)", hal::percpu::cpu_count()));
    // CP-4/CP-6: one line per CPU (topology, capacity, ready time, TSC offset, MTRR outcome).
    out.extend(platform::smp::summary());
    let f = hal::cpu::detect();
    // Interrupts off for the few MSR reads: no preemption, so the banks and the CPU number belong together.
    let (cpu, m) = {
        let _irq = hal::cpu::IrqGuard::new();
        (hal::percpu::this_cpu(), hal::cpu::mca_report(&f))
    };
    if !m.supported {
        out.push(String::from("mca: not supported"));
    } else {
        out.push(format!(
            "mca cpu={cpu} (only the CPU this call ran on; the banks are per core) banks={} mcg_cap={:#x} mcg_status={:#x} valid_records={}",
            m.banks,
            m.mcg_cap,
            m.mcg_status,
            m.records.len()
        ));
        for r in &m.records {
            let opt = |v: Option<u64>| v.map(|x| format!("{x:#x}")).unwrap_or_else(|| String::from("-"));
            out.push(format!(
                "mca bank={} status={:#018x} uc={} code={:#06x} addr={} misc={} ipid={}",
                r.bank,
                r.status,
                r.uncorrected(),
                r.mca_code(),
                opt(r.addr),
                opt(r.misc),
                opt(r.ipid)
            ));
        }
    }
    out
}

const KIND_NAMES: [&str; mm_core::KINDS] = ["heap", "page_table", "user", "dma", "stack", "page_cache", "driver", "other"];

fn mib(b: u64) -> String {
    format!("{}.{} MiB", b >> 20, ((b & 0xF_FFFF) * 10) >> 20)
}

fn memory() -> Vec<String> {
    let mut out = Vec::new();
    match mm_core::frames() {
        Some(fa) => {
            let s = fa.stats();
            out.push(format!("frames total={} free={} leaked={}", mib(s.total_bytes), mib(s.free_bytes), mib(s.leaked_bytes)));
            let used: Vec<String> = s.used_by_kind.iter().zip(KIND_NAMES).filter(|(b, _)| **b > 0).map(|(b, n)| format!("{n}={}", mib(*b))).collect();
            out.push(format!("frames used: {}", if used.is_empty() { String::from("-") } else { used.join(" ") }));
            // MM-9 CR4: page-cache reclaim at a glance (watermarks in KiB, direct reclaim, reclaimed pages).
            let w = mm_core::reclaim::watermarks(s.total_bytes / mm_core::PAGE);
            let (d, r) = (mm_core::reclaim::direct_stats(), vfs::reclaim::stats());
            let kib = |p: u64| p * (mm_core::PAGE / 1024);
            out.push(format!(
                "reclaim min={}K low={}K high={}K available={} direct runs={} pages={} failed={} dips={} reclaimed_pages={} preloaded_taken={} caches={} with_preloaded={}",
                kib(w.min),
                kib(w.low),
                kib(w.high),
                mib(vfs::reclaim::mem_available_bytes()),
                d.runs,
                d.pages,
                d.failed,
                d.dips,
                r.pages,
                r.preloaded_pages,
                r.listed.0,
                r.listed.1
            ));
        }
        None => out.push(String::from("frames: no frame allocator installed")),
    }
    out.push(format!("heap allocated={}", mib(mm_core::HEAP.allocated_bytes())));
    match acpi::live::memory_map() {
        Some(m) if !m.is_empty() => {
            let ram: u64 = m.regions().iter().filter(|r| acpi::memmap::is_ram(r.ty)).map(|r| r.end - r.start).sum();
            let top = m.regions().iter().filter(|r| acpi::memmap::is_ram(r.ty)).map(|r| r.end).max().unwrap_or(0);
            out.push(format!("memory map regions={} ram={} ram_top={top:#x}", m.regions().len(), mib(ram)));
            for r in m.regions() {
                out.push(format!("  {:#014x}-{:#014x} type={:#x}{}", r.start, r.end, r.ty, if acpi::memmap::is_ram(r.ty) { " ram" } else { "" }));
            }
        }
        _ => out.push(String::from("memory map: not known (acpi::init was not called)")),
    }
    out
}

fn section(name: &str, now_ns: u64) -> Vec<String> {
    match name {
        "kernel" => crate::boot::lines(now_ns),
        "cpu" => cpu(),
        "memory" => memory(),
        "firmware" => acpi::live::summary(),
        "interrupts" => acpi::irq::summary(),
        // PC-3 (granted to PC): every MSI/MSI-X set with its per-vector CPU, vector and delivery count.
        "msi" => pci::irq::summary(),
        _ => {
            let l = pci::tree_lines();
            if l.is_empty() { alloc::vec![String::from("pci: no devices (pci::scan has not run)")] } else { l }
        }
    }
}

fn hw_info(ctx: &ToolContext, args: &Json) -> ToolResult {
    let pick = match opt_str(args, "section")? {
        None | Some("all") => SECTIONS.to_vec(),
        Some(s) if SECTIONS.contains(&s) => alloc::vec![s],
        Some(_) => return Err(bad("section")),
    };
    let mut out = Vec::new();
    for s in pick {
        out.push(format!("== {s} =="));
        out.extend(section(s, ctx.now_ns()));
    }
    text(out)
}

mcp_tool!(
    name: "hw_info",
    owner: "MC",
    safety: Read,
    description: "The machine as the kernel sees it, by section: kernel (version, system generation and why it was started, Reload counter, which generation the next start brings, MCP request counters), cpu (CPUID facts, control registers, machine-check banks), memory (frames, heap, firmware memory map), firmware (ACPI tables), interrupts (APIC, I/O APICs), msi (MSI/MSI-X vectors per device: CPU, vector, delivery count, storms), pci (device tree). Without section: all.",
    schema: r#"{"type":"object","properties":{"section":{"type":"string","enum":["all","kernel","cpu","memory","firmware","interrupts","msi","pci"]}}}"#,
    handler: hw_info,
);
