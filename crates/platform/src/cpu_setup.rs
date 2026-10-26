//! CP-1, kernel side: what needs memory and the log after hal's early CPU set-up (`hal::cpu::init_bsp`, run
//! by main before memory exists). One Core initcall:
//! - moves the IST stacks (#DF, NMI, #MC) to guarded stacks in the CPU area (`hal::gdt::install_guarded_ist`),
//!   falling back to the boot stacks with a warning,
//! - logs the CPU facts and the set-up (`cpu.setup`),
//! - reads the machine-check banks once, read-only, and logs every valid record (`cpu.mca.*`).

use alloc::string::String;
use hal::cpu::{self, McaRecord};
use klog::Hex;

kreg::initcall!(kreg::Level::Core, "cpu.setup", setup);

fn setup() -> Result<(), &'static str> {
    let f = cpu::detect();
    log_setup(&f);
    install_ist();
    log_mca(&f);
    Ok(())
}

fn log_setup(f: &cpu::Features) {
    let facts = cpu::facts();
    let s = cpu::bsp_setup();
    klog::event!(Info, "cpu", "setup",
        vendor = facts.vendor.as_str(), brand = facts.brand.as_str(),
        family = Hex(facts.family as u64), model = Hex(facts.model as u64), stepping = facts.stepping,
        cr0 = Hex(s.cr0), cr0_was = Hex(s.cr0_before), cr4 = Hex(s.cr4), cr4_was = Hex(s.cr4_before),
        efer = Hex(s.efer), xcr0 = Hex(s.xcr0), xcr0_supported = Hex(facts.xcr0_supported),
        xsave_bytes = facts.xsave_size, xsaves_bytes = facts.xsaves_size,
        avx512 = f.avx512f, pcid = f.pcid, invpcid = f.invpcid, fsgsbase = f.fsgsbase,
        smep = f.smep, smap = f.smap, umip = f.umip, x2apic = f.x2apic, pku = f.pku, shstk = f.shstk, ibt = f.ibt,
        hypervisor = f.hypervisor);
    // The full table once, for the Mini-PC check list (AVX-512, PCID, INVPCID and the rest).
    let mut on = String::new();
    for (name, _) in facts.flags.iter().filter(|(_, v)| *v) {
        if !on.is_empty() {
            on.push(' ');
        }
        on.push_str(name);
    }
    klog::event!(Info, "cpu", "flags", count = facts.flags.iter().filter(|(_, v)| *v).count(), on = on);
    if s.u_cet_before != 0 || s.s_cet_before != 0 {
        klog::event!(Warn, "cpu", "cet.was_on", u_cet_was = Hex(s.u_cet_before), s_cet_was = Hex(s.s_cet_before);
            "firmware left CET enables in IA32_U_CET/IA32_S_CET; zeroed (CET stays off)");
    }
    if s.cr0_before & (cpu::CR0_CD | cpu::CR0_NW) != 0 {
        klog::event!(Warn, "cpu", "cache.was_off", cr0_was = Hex(s.cr0_before); "firmware left CR0.CD/NW set; cleared");
    }
}

fn install_ist() {
    let mut stack_page = || mm_core::frames()?.alloc(0, mm_core::Zone::Normal, mm_core::Kind::Stack);
    let mut free_page = |phys: u64| {
        if let Some(fa) = mm_core::frames() {
            fa.free(phys, 0, mm_core::Kind::Stack);
        }
    };
    let mut table = mm_core::alloc_table_frame;
    match hal::gdt::install_guarded_ist(&mut stack_page, &mut free_page, &mut table) {
        Ok(st) => klog::event!(Info, "cpu", "ist.guarded",
            df_top = Hex(st[0].top), nmi_top = Hex(st[1].top), mc_top = Hex(st[2].top),
            stack_kib = hal::gdt::IST_STACK_PAGES * 4, guard_pages = hal::gdt::IST_GUARD_PAGES),
        Err(e) => klog::event!(Warn, "cpu", "ist.fallback", error = alloc::format!("{e:?}");
            "IST stacks stay on the boot stacks without guard pages; the stack pages taken were freed"),
    }
}

fn log_mca(f: &cpu::Features) {
    let r = cpu::mca_report(f);
    if !r.supported {
        klog::event!(Info, "cpu", "mca.absent");
        return;
    }
    let readable = cpu::mca_readable_banks(r.banks, f.smca);
    klog::event!(Info, "cpu", "mca.banks", banks = r.banks, readable = readable, mcg_cap = Hex(r.mcg_cap),
        mcg_status = Hex(r.mcg_status), valid = r.records.len(), smca = f.smca);
    if readable < r.banks {
        klog::event!(Warn, "cpu", "mca.banks_unreadable", banks = r.banks, readable = readable;
            "legacy MCA layout has addresses for 32 banks only; the rest are not read");
    }
    for rec in &r.records {
        log_record(rec);
    }
}

fn log_record(r: &McaRecord) {
    // Records found at boot are history (firmware, last boot); uncorrected ones are worth a warning.
    if r.uncorrected() {
        klog::event!(Warn, "cpu", "mca.record", bank = r.bank, status = Hex(r.status), code = Hex(r.mca_code() as u64),
            uncorrected = true, addr = Hex(r.addr.unwrap_or(0)), misc = Hex(r.misc.unwrap_or(0)), ipid = Hex(r.ipid.unwrap_or(0)));
    } else {
        klog::event!(Info, "cpu", "mca.record", bank = r.bank, status = Hex(r.status), code = Hex(r.mca_code() as u64),
            uncorrected = false, addr = Hex(r.addr.unwrap_or(0)), misc = Hex(r.misc.unwrap_or(0)), ipid = Hex(r.ipid.unwrap_or(0)));
    }
}
