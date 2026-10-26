//! Decisions about the CPU from register values (the binary reads the registers).

use alloc::vec::Vec;

pub const CR4_LA57: u64 = 1 << 12;
pub const CR4_CET: u64 = 1 << 23;

/// The firmware's paging mode: BitsOS V2 builds 4-level tables, so 5-level paging is refused (`spec/core.md`
/// §2.3); leaving LA57 needs paging off, which a loader running on the firmware's tables cannot do.
pub fn paging_refusal(cr4: u64) -> Option<&'static str> {
    (cr4 & CR4_LA57 != 0).then_some(
        "the firmware runs with 5-level paging (CR4.LA57); BitsOS V2 needs 4-level paging. \
         Turn off 5-level paging in the firmware setup if it offers that.",
    )
}

/// CPUID words the x86-64-v3 check needs.
#[derive(Clone, Copy, Default)]
pub struct CpuidWords {
    /// Leaf 1 ECX and EDX.
    pub ecx1: u32,
    pub edx1: u32,
    /// Leaf 7 sub-leaf 0 EBX (0 when leaf 7 does not exist).
    pub ebx7: u32,
    /// Leaf 0x8000_0001 ECX and EDX (0 when that leaf does not exist).
    pub ecx81: u32,
    pub edx81: u32,
}

/// x86-64-v3 (rule R10) plus NX: the names of what this CPU lacks. XSAVE is checked, OSXSAVE is not: that
/// bit mirrors CR4.OSXSAVE, which the kernel sets (CP-1); firmware often leaves it clear.
pub fn missing_v3(w: CpuidWords) -> Vec<&'static str> {
    let need: [(&str, u32, u32); 18] = [
        ("SSE2", w.edx1, 26),
        ("SSE3", w.ecx1, 0),
        ("SSSE3", w.ecx1, 9),
        ("FMA", w.ecx1, 12),
        ("CMPXCHG16B", w.ecx1, 13),
        ("SSE4.1", w.ecx1, 19),
        ("SSE4.2", w.ecx1, 20),
        ("MOVBE", w.ecx1, 22),
        ("POPCNT", w.ecx1, 23),
        ("XSAVE", w.ecx1, 26),
        ("AVX", w.ecx1, 28),
        ("F16C", w.ecx1, 29),
        ("BMI1", w.ebx7, 3),
        ("AVX2", w.ebx7, 5),
        ("BMI2", w.ebx7, 8),
        ("LAHF/SAHF", w.ecx81, 0),
        ("LZCNT", w.ecx81, 5),
        ("NX", w.edx81, 20),
    ];
    need.iter().filter(|(_, reg, bit)| reg & (1 << bit) == 0).map(|(name, _, _)| *name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la57_is_refused_readably() {
        assert!(paging_refusal(0x668).is_none());
        let m = paging_refusal(0x668 | CR4_LA57).unwrap();
        assert!(m.contains("5-level") && m.contains("LA57"));
    }

    #[test]
    fn v3_complete_and_missing() {
        let all = CpuidWords {
            ecx1: (1 << 0) | (1 << 9) | (1 << 12) | (1 << 13) | (1 << 19) | (1 << 20) | (1 << 22) | (1 << 23) | (1 << 26) | (1 << 28) | (1 << 29),
            edx1: 1 << 26,
            ebx7: (1 << 3) | (1 << 5) | (1 << 8),
            ecx81: 1 | (1 << 5),
            edx81: 1 << 20,
        };
        assert!(missing_v3(all).is_empty());
        // OSXSAVE (ECX bit 27) is not required.
        assert!(missing_v3(CpuidWords { ecx1: all.ecx1 & !(1 << 27), ..all }).is_empty());
        let old = CpuidWords { ebx7: 0, ..all };
        assert_eq!(missing_v3(old), ["BMI1", "AVX2", "BMI2"]);
        assert_eq!(missing_v3(CpuidWords::default()).len(), 18);
    }
}
