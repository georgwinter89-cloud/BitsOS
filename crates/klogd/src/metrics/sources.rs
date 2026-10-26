//! The kernel's numbers for one round: scheduler (per CPU, SC-6's user split), programs (thread-group CPU
//! time), the frame pool, network cards, disks, every `klog::counter!`, k10temp sensors and RAPL.

use super::sampler::{CpuRaw, DiskRaw, NetRaw, ProcRaw, Reading};
use super::temp::{Probe, Zen};
use alloc::string::String;
use hal::msr::{AMD_PKG_ENERGY_STATUS, AMD_RAPL_POWER_UNIT, read_checked};

/// k10temp sensors: found once the kernel has scanned PCI (after the init levels, so after the sampler
/// started), or why there are none.
pub enum Sensors {
    Pending((Option<u32>, u32)),
    Ready(Zen),
    Absent(&'static str),
}

/// What the sources found once (sensors, energy unit), kept between rounds.
pub struct Sources {
    pub zen: Sensors,
    /// RAPL energy status unit (bits 12:8 of the power unit MSR, linux-6.6/arch/x86/events/rapl.c), or why not.
    pub esu: Result<u32, &'static str>,
}

impl Sources {
    pub fn detect() -> Sources {
        let esu = read_checked(AMD_RAPL_POWER_UNIT).map(|u| ((u >> 8) & 0x1f) as u32).ok_or("no AMD RAPL (CPUID 0x80000007 EDX 14)");
        let zen = match Zen::cpu() {
            Ok(layout) => Sensors::Pending(layout),
            Err(why) => Sensors::Absent(why),
        };
        Sources { zen, esu }
    }

    /// Looks for the sensors while they are pending; true when they were just decided.
    pub fn find_sensors(&mut self) -> bool {
        let Sensors::Pending(layout) = self.zen else { return false };
        if pci::devices().is_empty() {
            return false;
        }
        self.zen = match Zen::on_root(layout) {
            Ok(z) => Sensors::Ready(z),
            Err(Probe::Again) => return false,
            Err(Probe::Absent(why)) => Sensors::Absent(why),
        };
        true
    }

    pub fn read(&self, now_ns: u64, cost_ns: u64) -> Reading {
        let cpus = sched::cpu_stats()
            .into_iter()
            .filter(|c| c.online)
            .map(|c| CpuRaw { cpu: c.cpu, busy_ns: c.busy_ns, idle_ns: c.idle_ns, user_ns: c.user_ns })
            .collect();
        let procs = proc::processes().iter().map(|p| ProcRaw { pid: p.pid, name: p.name.clone(), cpu_ns: p.cpu_ns() }).collect();
        let mem = mm_core::frames().map(|f| {
            let s = f.stats();
            (s.total_bytes, s.free_bytes, s.used_by_kind)
        });
        let nets = net::netdev::devices()
            .iter()
            .map(|d| {
                let s = d.stats();
                NetRaw { name: String::from(d.name()), totals: [s.rx_bytes, s.tx_bytes, s.rx_frames, s.tx_frames, s.rx_dropped, s.tx_dropped] }
            })
            .collect();
        let disks = block::disks()
            .iter()
            .map(|d| {
                let s = d.stats();
                DiskRaw { name: String::from(d.name()), requests: s.requests, dispatches: s.dispatches, errors: s.errors, timeouts: s.timeouts, queued: s.queued, read_bytes: s.read_bytes, write_bytes: s.write_bytes, busy_ns: s.busy_ns }
            })
            .collect();
        let temps = match &self.zen {
            Sensors::Ready(z) => z.read(),
            _ => alloc::vec::Vec::new(),
        };
        let pkg_energy = match self.esu {
            Ok(esu) => read_checked(AMD_PKG_ENERGY_STATUS).map(|e| (e as u32, esu)),
            Err(_) => None,
        };
        Reading { now_ns, cpus, procs, mem, nets, disks, counters: klog::metrics::snapshot().collect(), temps, pkg_energy, cost_ns }
    }
}
