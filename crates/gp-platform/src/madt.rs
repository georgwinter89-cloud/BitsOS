//! Owner: wave GP-CPU (plan/spec/gaming-pc.md §4). The empty-slot rule for x2APIC entries and a byte builder of
//! the Gaming-PC's MADT shape, so `acpi` can prove the rule on this machine's table without a capture in git.
//!
//! The firmware fills its 48 processor slots with 20 real CPUs and 28 entries of ID 0xFFFFFFFF with flags 0. Linux
//! drops such an entry without a word (linux-7.3.1/arch/x86/kernel/acpi/boot.c:199-201; an invalid ID that is
//! enabled gets "x2apic entry ignored", :222-226); BitsOS reported 28
//! "problems", which hides real ones.

/// The x2APIC ID that addresses every CPU (boot.c:200).
pub const X2APIC_BROADCAST: u32 = 0xFFFF_FFFF;
/// Local APIC flags (linux-7.3.1/include/acpi/actbl2.h:1801-1802).
pub const ENABLED: u32 = 1;
pub const ONLINE_CAPABLE: u32 = 2;

/// An x2APIC entry (type 9) that is an unused slot, not a CPU and not a problem: the broadcast ID and neither
/// enabled nor able to come online. The same ID with a flag set stays a firmware problem worth a line.
pub fn is_placeholder(x2apic_id: u32, flags: u32) -> bool {
    x2apic_id == X2APIC_BROADCAST && flags & (ENABLED | ONLINE_CAPABLE) == 0
}

/// The Gaming-PC's CPUs in the table's order as (ACPI UID, x2APIC ID): eight P cores, then twelve E cores
/// (`plan/spec/gaming-pc.md` section 1, read from the machine on 5.11.2026).
pub const GAMING_PC_CPUS: [(u32, u32); 20] = [
    (0, 0), (1, 8), (6, 32), (7, 40), (8, 48), (9, 56), (18, 80), (19, 88),
    (2, 24), (3, 26), (4, 28), (5, 30),
    (10, 64), (11, 66), (12, 68), (13, 70), (14, 72), (15, 74), (16, 76), (17, 78),
];
/// The unused slots: UIDs 0x14..=0x2F.
pub const GAMING_PC_PLACEHOLDERS: core::ops::RangeInclusive<u32> = 0x14..=0x2F;

/// Builds MADT bytes (ACPI 6.5 §5.2.12) with a valid checksum.
pub struct MadtBuilder {
    body: alloc::vec::Vec<u8>,
}

impl MadtBuilder {
    pub fn new(lapic_address: u32, flags: u32) -> MadtBuilder {
        let mut body = alloc::vec::Vec::new();
        body.extend_from_slice(&lapic_address.to_le_bytes());
        body.extend_from_slice(&flags.to_le_bytes());
        MadtBuilder { body }
    }
    /// Type 9, 16 bytes: reserved, x2APIC ID, flags, UID.
    pub fn x2apic(mut self, uid: u32, id: u32, flags: u32) -> MadtBuilder {
        self.body.extend_from_slice(&[9, 16, 0, 0]);
        for v in [id, flags, uid] {
            self.body.extend_from_slice(&v.to_le_bytes());
        }
        self
    }
    /// Type 1, 12 bytes.
    pub fn ioapic(mut self, id: u8, address: u32, gsi_base: u32) -> MadtBuilder {
        self.body.extend_from_slice(&[1, 12, id, 0]);
        self.body.extend_from_slice(&address.to_le_bytes());
        self.body.extend_from_slice(&gsi_base.to_le_bytes());
        self
    }
    /// Type 2, 10 bytes.
    pub fn irq_override(mut self, source: u8, gsi: u32, flags: u16) -> MadtBuilder {
        self.body.extend_from_slice(&[2, 10, 0, source]);
        self.body.extend_from_slice(&gsi.to_le_bytes());
        self.body.extend_from_slice(&flags.to_le_bytes());
        self
    }
    /// Type 0xA, 12 bytes: flags, UID, LINT, three reserved bytes.
    pub fn x2apic_nmi(mut self, uid: u32, flags: u16, lint: u8) -> MadtBuilder {
        self.body.extend_from_slice(&[0xA, 12]);
        self.body.extend_from_slice(&flags.to_le_bytes());
        self.body.extend_from_slice(&uid.to_le_bytes());
        self.body.extend_from_slice(&[lint, 0, 0, 0]);
        self
    }
    /// The whole table: a 36-byte header (revision 5, the board's OEM strings) and the body.
    pub fn build(self) -> alloc::vec::Vec<u8> {
        let mut t = alloc::vec::Vec::with_capacity(36 + self.body.len());
        t.extend_from_slice(b"APIC");
        t.extend_from_slice(&((36 + self.body.len()) as u32).to_le_bytes());
        t.extend_from_slice(&[5, 0]);
        t.extend_from_slice(b"ALASKA");
        t.extend_from_slice(b"A M I \0\0");
        t.extend_from_slice(&0x0107_2009u32.to_le_bytes());
        t.extend_from_slice(b"AMI ");
        t.extend_from_slice(&0x0100_0013u32.to_le_bytes());
        t.extend_from_slice(&self.body);
        let sum = t.iter().fold(0u8, |a, b| a.wrapping_add(*b));
        t[9] = 0u8.wrapping_sub(sum);
        t
    }
}

/// The Gaming-PC's MADT shape: 856 bytes, 20 CPUs and 28 empty slots as x2APIC entries, one I/O APIC, two
/// overrides, one x2APIC NMI for all CPUs, in the firmware's order (the CPUs, then the slots). Byte for byte the
/// machine's table: `acpi`'s test compares it with the captured one.
pub fn gaming_pc() -> alloc::vec::Vec<u8> {
    let mut b = MadtBuilder::new(0xFEE0_0000, 1);
    for (uid, id) in GAMING_PC_CPUS {
        b = b.x2apic(uid, id, ENABLED);
    }
    for uid in GAMING_PC_PLACEHOLDERS {
        b = b.x2apic(uid, X2APIC_BROADCAST, 0);
    }
    b.ioapic(2, 0xFEC0_0000, 0).irq_override(0, 2, 0).irq_override(9, 9, 0x000D).x2apic_nmi(0xFFFF_FFFF, 0x000D, 1).build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_rule() {
        assert!(is_placeholder(X2APIC_BROADCAST, 0));
        assert!(is_placeholder(X2APIC_BROADCAST, 0xFFFF_FFFC), "reserved flag bits do not make a CPU");
        assert!(!is_placeholder(X2APIC_BROADCAST, ENABLED), "an enabled broadcast ID is a firmware problem");
        assert!(!is_placeholder(X2APIC_BROADCAST, ONLINE_CAPABLE));
        assert!(!is_placeholder(0, 0), "a disabled CPU with a real ID is a CPU");
        assert!(!is_placeholder(0xFF, 0), "0xFF is a valid x2APIC ID");
        assert!(!is_placeholder(0xFFFF_FFFE, 0));
    }

    #[test]
    fn the_gaming_pcs_shape() {
        let t = gaming_pc();
        assert_eq!(t.len(), 856, "the machine's table is 856 bytes");
        assert_eq!(&t[0..4], b"APIC");
        assert_eq!(u32::from_le_bytes(t[4..8].try_into().unwrap()), 856);
        assert_eq!(t.iter().fold(0u8, |a, b| a.wrapping_add(*b)), 0, "checksum");
        // Walk the entries the way a parser does.
        let (mut o, mut cpus, mut empty, mut other) = (44, 0, 0, alloc::vec::Vec::new());
        while o < t.len() {
            let (ty, len) = (t[o], t[o + 1] as usize);
            if ty == 9 {
                let id = u32::from_le_bytes(t[o + 4..o + 8].try_into().unwrap());
                let flags = u32::from_le_bytes(t[o + 8..o + 12].try_into().unwrap());
                if is_placeholder(id, flags) { empty += 1 } else { cpus += 1 }
            } else {
                other.push(ty);
            }
            o += len;
        }
        assert_eq!(o, t.len());
        assert_eq!((cpus, empty), (20, 28));
        assert_eq!(other, [1, 2, 2, 0xA]);
    }

    #[test]
    fn the_cpu_list_is_the_machines() {
        let mut ids: alloc::vec::Vec<u32> = GAMING_PC_CPUS.iter().map(|c| c.1).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 20, "20 different APIC IDs");
        let mut uids: alloc::vec::Vec<u32> = GAMING_PC_CPUS.iter().map(|c| c.0).collect();
        uids.sort_unstable();
        assert_eq!(uids, (0..20).collect::<alloc::vec::Vec<u32>>(), "UIDs 0..19, the slots follow at 0x14");
        // P cores have IDs that are multiples of 8, E cores sit two apart in fours.
        assert!(GAMING_PC_CPUS[..8].iter().all(|c| c.1 % 8 == 0));
        assert!(GAMING_PC_CPUS[8..].chunks(4).all(|c| c.windows(2).all(|w| w[1].1 == w[0].1 + 2)));
    }
}
