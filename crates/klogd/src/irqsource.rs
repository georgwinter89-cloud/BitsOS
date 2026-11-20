//! GP-R3: which source owns a vector, for the storm line. The Mini-PC's check of generation 164 saw `storm.start
//! cpu=2 vector=242` and had to find out by hand that 0xF2 is the TLB shootdown IPI, not a device that missed its
//! acknowledge. The line now says it: a system vector by its owner (for the shootdown also what asked for the IPIs
//! in that look, from `tlb::causes`), a dynamic vector by the MSI set that holds it, else `dynamic`.

use alloc::format;
use alloc::string::String;
use hal::paging::tlb::{self, Cause, CAUSES};

/// The owner of a system vector (CP-3's range 0xF0-0xFE plus the spurious one). `resched` is sched's vector (taken
/// at boot from 0xF2 up, so it is read, not fixed).
pub fn system_name(vector: u8, resched: u8) -> Option<&'static str> {
    Some(match vector {
        v if v == hal::vectors::LAPIC_TIMER => "lapic_timer",
        v if v == hal::vectors::SMP_CALL => "smp_call",
        v if v == tlb::VECTOR => "tlb_shootdown",
        v if v == hal::kexec_stub::PARK_VECTOR => "park",
        v if v == hal::vectors::SPURIOUS_VECTOR => "spurious",
        v if resched != 0 && v == resched => "resched",
        v if (hal::vectors::FIRST_SYSTEM..=hal::vectors::LAST_SYSTEM).contains(&v) => "system",
        _ => return None,
    })
}

/// The cause that sent the most shootdown IPIs between two snapshots of `tlb::causes` (IPIs per cause), with its
/// count; `None` when no cause sent any in between. The counters are machine-wide: the count is every IPI that cause
/// sent to any CPU in that look, not only those to the CPU that stormed.
pub fn top_cause(before: &[u64; CAUSES], now: &[u64; CAUSES]) -> Option<(&'static str, u64)> {
    Cause::ALL
        .iter()
        .map(|c| (c.name(), now[*c as usize].saturating_sub(before[*c as usize])))
        .filter(|x| x.1 > 0)
        .max_by_key(|x| x.1)
}

/// An MSI set's place for `cpu`/`vector`: `msi:<function>/<driver>#<index>`.
pub fn msi_owner<'a>(sets: impl IntoIterator<Item = (String, &'a str, Option<(u32, u8)>, usize)>, cpu: u32, vector: u8) -> Option<String> {
    sets.into_iter().find(|s| s.2 == Some((cpu, vector))).map(|(at, name, _, i)| format!("msi:{at}/{name}#{i}"))
}

/// IPIs per cause now (the snapshot `top_cause` compares).
pub fn tlb_ipis() -> [u64; CAUSES] {
    tlb::causes().map(|c| c.ipis)
}

/// Kernel glue: `source` and `cause` fields for a storm on `cpu`/`vector`; `ipis` the two snapshots of this look.
/// `cause` is machine-wide for that look (`top_cause`): it names what asked for shootdowns, not how many hit `cpu`.
pub fn describe(cpu: u32, vector: u8, ipis: Option<(&[u64; CAUSES], &[u64; CAUSES])>) -> (String, String) {
    if let Some(n) = system_name(vector, sched::resched_vector()) {
        let cause = match (vector == tlb::VECTOR, ipis.and_then(|(a, b)| top_cause(a, b))) {
            (true, Some((name, n))) => format!("{name}:{n}"),
            _ => String::from("-"),
        };
        return (String::from(n), cause);
    }
    let sets = pci::irq::sets().into_iter().flat_map(|(at, name, _, stats)| {
        stats.into_iter().enumerate().map(move |(i, s)| (format!("{at}"), name, s.vector.map(|v| (v.cpu, v.vector)), i)).collect::<alloc::vec::Vec<_>>()
    });
    (msi_owner(sets, cpu, vector).unwrap_or_else(|| String::from("dynamic")), String::from("-"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn the_desks_vector_242_is_the_tlb_shootdown() {
        assert_eq!(tlb::VECTOR, 242);
        assert_eq!(system_name(242, 0xF3), Some("tlb_shootdown"));
        assert_eq!(system_name(0xF3, 0xF3), Some("resched"));
        assert_eq!(system_name(0xF0, 0xF3), Some("lapic_timer"));
        assert_eq!(system_name(0xF1, 0xF3), Some("smp_call"));
        assert_eq!(system_name(0xFC, 0xF3), Some("park"));
        assert_eq!(system_name(0xFE, 0xF3), Some("system"));
        assert_eq!(system_name(0x41, 0xF3), None, "a dynamic vector is no system one");
        assert_eq!(system_name(0xF3, 0), Some("system"), "before sched registered its vector");
    }

    #[test]
    fn the_cause_that_sent_most_in_the_look() {
        let mut a = [0u64; CAUSES];
        let mut b = [0u64; CAUSES];
        a[Cause::Munmap as usize] = 1000;
        b[Cause::Munmap as usize] = 6600;
        b[Cause::Mprotect as usize] = 300;
        assert_eq!(top_cause(&a, &b), Some(("munmap", 5600)));
        assert_eq!(top_cause(&b, &b), None);
    }

    #[test]
    fn an_msi_vector_is_named_by_its_set() {
        let sets = vec![
            (String::from("83:00.0"), "rtl8126", Some((4u32, 0x41u8)), 0usize),
            (String::from("01:00.0"), "nvme", Some((2, 0x42)), 3),
            (String::from("01:00.0"), "nvme", None, 4),
        ];
        assert_eq!(msi_owner(sets.clone(), 2, 0x42).as_deref(), Some("msi:01:00.0/nvme#3"));
        assert_eq!(msi_owner(sets.clone(), 4, 0x41).as_deref(), Some("msi:83:00.0/rtl8126#0"));
        assert_eq!(msi_owner(sets, 3, 0x42), None, "the vector on another CPU is another one");
    }
}
