//! Which deep states an idle CPU may use (pure; host-tested). The rules are Linux's for ACPI `_CST` states
//! (linux-6.6/drivers/acpi/processor_idle.c `acpi_processor_power_verify` l.410-460, `verify_c3` l.310-390,
//! `lapic_timer_check_state` l.125-146, `tsc_check_state` l.186-206; arch/x86/kernel/acpi/cstate.c
//! `acpi_processor_power_init_bm_check` l.29-104), in BitsOS's shape: where Linux would need a broadcast timer
//! or an unstable-TSC fallback, BitsOS stays in C1 altogether.

use acpi::cst::{CState, Entry};
use alloc::vec::Vec;

/// Exit latencies above this are taken as a table error (ACPI's own FADT limits are 100 us for C2 and 1 ms
/// for C3; `_CST` has none, 10 ms leaves room for any real state).
pub const MAX_LATENCY_US: u32 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vendor {
    Amd,
    Intel,
    Other,
}

/// What the plan needs to know about the CPU (the boot CPU's CPUID; all CPUs of one machine alike).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuIdle {
    /// CPUID.1:ECX bit 31: under a hypervisor every port read traps; Linux's guests use hlt too.
    pub hypervisor: bool,
    /// CPUID.6:EAX bit 2: the LAPIC timer keeps running in deep C-states.
    pub arat: bool,
    /// CPUID.80000007h:EDX bit 8: the TSC ticks on in every C-state (Linux `nonstop_tsc`).
    pub invariant_tsc: bool,
    pub vendor: Vendor,
    pub family: u32,
    pub model: u32,
}

impl CpuIdle {
    /// The Mini-PC's Ryzen 7 PRO 8845HS (family 0x19 model 0x75), for tests.
    pub const ZEN4: CpuIdle =
        CpuIdle { hypervisor: false, arat: true, invariant_tsc: true, vendor: Vendor::Amd, family: 0x19, model: 0x75 };

    /// This CPU.
    pub fn live() -> CpuIdle {
        let v = hal::cpu::live();
        let f = hal::cpu::detect();
        let (family, model, _) = hal::cpu::decode_fms(hal::cpu::cpuid(1, 0).eax);
        CpuIdle {
            hypervisor: hal::serial::under_hypervisor(),
            arat: v.has("arat"),
            invariant_tsc: v.has("constant_tsc"),
            vendor: if f.amd {
                Vendor::Amd
            } else if f.intel {
                Vendor::Intel
            } else {
                Vendor::Other
            },
            family,
            model,
        }
    }

    /// This CPU's own ARAT and invariant-TSC bits straight from CPUID (no allocation; every AP at bring-up).
    pub fn timer_bits_this_cpu() -> (bool, bool) {
        let c = hal::cpu::cpuid;
        let arat = c(0, 0).eax >= 6 && c(6, 0).eax & (1 << 2) != 0;
        let tsc = c(0x8000_0000, 0).eax >= 0x8000_0007 && c(0x8000_0007, 0).edx & (1 << 8) != 0;
        (arat, tsc)
    }

    /// Linux `bm_check`: the CPU keeps its caches coherent in C3, so C3 needs no cache flush but a look at
    /// the bus-master status first. AMD Zen (family 0x17+) and Intel since Core 2: yes, and neither needs
    /// ARB_DIS (`bm_control` 0). Everything else would need WBINVD before C3, which BitsOS does not do.
    pub fn bm_check(&self) -> bool {
        match self.vendor {
            Vendor::Amd => self.family >= 0x17,
            Vendor::Intel => self.family > 0xF || (self.family == 6 && self.model >= 0x0F),
            Vendor::Other => false,
        }
    }
}

/// What the firmware gives.
#[derive(Clone, Copy, Debug, Default)]
pub struct Firmware<'a> {
    /// The `_CST` states (empty: none found).
    pub states: &'a [CState],
    /// The ACPI PM timer port (Linux's "dummy wait" read after the C-state port, on Intel only).
    pub pm_timer: Option<u16>,
    /// The PM1a and PM1b event blocks: their first word is PM1 status with BM_STS in bit 4.
    pub pm1_status: [Option<u16>; 2],
}

/// Why the idle loop stays in C1 altogether.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum C1Only {
    Hypervisor,
    /// Linux would broadcast the timer from the HPET; BitsOS stays shallow.
    NoArat,
    /// Linux would mark the TSC unstable in deep states; BitsOS's clock needs it.
    NoInvariantTsc,
    NoCst,
    NoUsableState,
}

impl C1Only {
    pub fn name(self) -> &'static str {
        match self {
            C1Only::Hypervisor => "hypervisor",
            C1Only::NoArat => "no_arat",
            C1Only::NoInvariantTsc => "no_invariant_tsc",
            C1Only::NoCst => "no_cst",
            C1Only::NoUsableState => "no_usable_state",
        }
    }
}

/// Why one `_CST` state is not used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Type 0 or above 3.
    Kind,
    /// FFH (MWAIT) deeper than C1: BitsOS enters no MWAIT states.
    Ffh,
    /// Neither system I/O nor FFH.
    Space,
    PortZero,
    /// Past 0xFFFF.
    PortRange,
    /// 0 or above `MAX_LATENCY_US`.
    Latency,
    /// The ACPI register guard refused the port (it names a disk controller, the PCI pair, ...).
    Guard,
    /// The same port as a state already taken.
    Duplicate,
    /// C3 on a CPU that would need a cache flush before it.
    C3NeedsFlush,
    /// C3 needs the bus-master status, but no PM1 status port is usable.
    C3NoBmStatus,
}

impl Refused {
    pub fn name(self) -> &'static str {
        match self {
            Refused::Kind => "kind",
            Refused::Ffh => "ffh",
            Refused::Space => "space",
            Refused::PortZero => "port_zero",
            Refused::PortRange => "port_range",
            Refused::Latency => "latency",
            Refused::Guard => "guard",
            Refused::Duplicate => "duplicate",
            Refused::C3NeedsFlush => "c3_needs_flush",
            Refused::C3NoBmStatus => "c3_no_bm_status",
        }
    }
}

/// One usable deep state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deep {
    pub kind: u8,
    pub port: u16,
    pub latency_ns: u64,
    /// Look at the bus-master status before entering (C3 with `bm_check`); busy -> a shallower state.
    pub bm_check: bool,
}

/// The idle plan: the deep states by rising exit latency and how to enter them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub deep: Vec<Deep>,
    /// Port read after the C-state port (Intel only, never under a hypervisor: Linux `io_idle`).
    pub dummy_read: Option<u16>,
    /// PM1 status ports for the bus-master check (only when a state needs it).
    pub bm_status: [Option<u16>; 2],
    pub c1_only: Option<C1Only>,
    pub refused: Vec<(CState, Refused)>,
    /// The table's latencies did not rise with the state depth; they were sorted (Linux "FW issue: working
    /// around C-state latencies out of order").
    pub latency_reordered: bool,
}

/// The plan for `cpu` and `fw` (`None`: no tables). `port_ok(port, bytes)` asks the ACPI register guard; it
/// is asked only for ports the plan would really touch.
pub fn plan(cpu: &CpuIdle, fw: Option<&Firmware<'_>>, port_ok: &mut dyn FnMut(u16, u32) -> bool) -> Plan {
    let c1 = |why| Plan { c1_only: Some(why), ..Plan::default() };
    if cpu.hypervisor {
        return c1(C1Only::Hypervisor);
    }
    if !cpu.arat {
        return c1(C1Only::NoArat);
    }
    if !cpu.invariant_tsc {
        return c1(C1Only::NoInvariantTsc);
    }
    let Some(fw) = fw.filter(|f| !f.states.is_empty()) else {
        return c1(C1Only::NoCst);
    };
    let mut p = Plan::default();
    let bm_check = cpu.bm_check();
    // Asked once, and only when a C3 needs it.
    let mut bm_ports: Option<Option<[Option<u16>; 2]>> = None;
    for c in fw.states {
        let r = judge(c, bm_check, &p.deep).and_then(|d| match d {
            Some(d) if d.bm_check && bm_ports.get_or_insert_with(|| bm_status_ports(fw.pm1_status, port_ok)).is_none() => {
                Err(Refused::C3NoBmStatus)
            }
            Some(d) if !port_ok(d.port, 1) => Err(Refused::Guard),
            d => Ok(d),
        });
        match r {
            Ok(Some(d)) => p.deep.push(d),
            Ok(None) => {}
            Err(why) => p.refused.push((*c, why)),
        }
    }
    p.deep.sort_by_key(|d| (d.kind, d.latency_ns));
    let mut lat: Vec<u64> = p.deep.iter().map(|d| d.latency_ns).collect();
    if lat.windows(2).any(|w| w[1] < w[0]) {
        lat.sort_unstable();
        for (d, l) in p.deep.iter_mut().zip(lat) {
            d.latency_ns = l;
        }
        p.latency_reordered = true;
    }
    if p.deep.iter().any(|d| d.bm_check) {
        p.bm_status = bm_ports.flatten().unwrap_or_default();
    }
    if cpu.vendor == Vendor::Intel {
        p.dummy_read = fw.pm_timer.filter(|&t| port_ok(t, 4));
    }
    if p.deep.is_empty() {
        p.c1_only = Some(C1Only::NoUsableState);
    }
    p
}

/// One state by itself: `Ok(None)` for C1 (entered by `hlt`), `Ok(Some)` for a deep state the table
/// describes well enough (the ports are checked by the caller).
fn judge(c: &CState, bm_check: bool, taken: &[Deep]) -> Result<Option<Deep>, Refused> {
    let addr = match (c.kind, c.entry) {
        (1, _) => return Ok(None),
        (0 | 4.., _) => return Err(Refused::Kind),
        (_, Entry::FixedHw(_)) => return Err(Refused::Ffh),
        (_, Entry::Other(_)) => return Err(Refused::Space),
        (_, Entry::Io(a)) => a,
    };
    if addr == 0 {
        return Err(Refused::PortZero);
    }
    let port = u16::try_from(addr).map_err(|_| Refused::PortRange)?;
    if c.latency_us == 0 || c.latency_us > MAX_LATENCY_US {
        return Err(Refused::Latency);
    }
    if taken.iter().any(|d| d.port == port) {
        return Err(Refused::Duplicate);
    }
    let c3 = c.kind == 3;
    if c3 && !bm_check {
        return Err(Refused::C3NeedsFlush);
    }
    Ok(Some(Deep { kind: c.kind, port, latency_ns: c.latency_us as u64 * 1000, bm_check: c3 }))
}

/// The PM1 status ports, if every one the FADT names may be read and written (BM_STS is write-1-to-clear).
/// `None` when PM1a is missing or a named port is refused: then the bus-master status cannot be known.
fn bm_status_ports(pm1: [Option<u16>; 2], port_ok: &mut dyn FnMut(u16, u32) -> bool) -> Option<[Option<u16>; 2]> {
    pm1[0]?;
    for p in pm1.iter().flatten() {
        if !port_ok(*p, 2) {
            return None;
        }
    }
    Some(pm1)
}
