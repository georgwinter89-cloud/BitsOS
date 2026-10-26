//! BAR5 facts outside the DCN header that the display driver meets (hand-written, not generated).
//! Sources: werkzeuge/plattform/gpu/doku/90-fallen.md R4, R6, R7, R22, R23 and 01-ip-register.md §4.

use core::ops::Range;

/// NBIO 7.7 indirect access (SMN) through BAR5; one lock across all users (X10).
pub const PCIE_INDEX2: u32 = 0x38;
pub const PCIE_DATA2: u32 = 0x3C;

/// DCN seg0 bytes: NBIO RSMU, several DCN registers per offset. Never touch as DCN (X8, R4).
pub const NBIO_RSMU_ALIAS: Range<u32> = 0x48..0x78;
pub const RSMU_INDEX: u32 = 0x50;
pub const RSMU_DATA: u32 = 0x54;

/// NBIO RCC_CONFIG (V1 `NB_MEMSIZE`, `NB_APERSIZE`): graphics memory and BAR0 size in MB.
pub const RCC_CONFIG_MEMSIZE: u32 = 0xDE3 * 4;
pub const RCC_CONFIG_APER_SIZE: u32 = 0xDE7 * 4;

/// Phoenix MP0/MP1 segment 1 (dwords). The pairing rule (R22, R23): mp_13_0_4 BASE_IDX 1 with the
/// Phoenix base list only; Rembrandt bases or mp_13_0_5's idx 0 with this list hit 0x4000 + offset.
pub const MP_SEG1_PHOENIX: u32 = 0x16000;
const MP0_C2PMSG_0: u32 = 0x40;
const MP1_C2PMSG_0: u32 = 0x240;

const fn mp0(n: u32) -> u32 {
    (MP_SEG1_PHOENIX + MP0_C2PMSG_0 + n) * 4
}
const fn mp1(n: u32) -> u32 {
    (MP_SEG1_PHOENIX + MP1_C2PMSG_0 + n) * 4
}

/// PSP ready (bit 31) and its heartbeat counter (test for non-zero, never compare).
pub const MP0_C2PMSG_33: u32 = mp0(33);
pub const MP0_C2PMSG_81: u32 = mp0(81);

/// DC's SMU mailbox: message 67, argument 83, response 91 (X14, X33).
pub const DC_SMU_MSG: u32 = mp1(67);
pub const DC_SMU_ARG: u32 = mp1(83);
pub const DC_SMU_RESP: u32 = mp1(91);

/// PMFW's own mailbox (66/82/90): never mixed with DC's (X15).
pub const PMFW_MSG: u32 = mp1(66);
pub const PMFW_ARG: u32 = mp1(82);
pub const PMFW_RESP: u32 = mp1(90);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measured_addresses() {
        // G1-BERICHT / spec gpu.md 2.2 and 2.4.
        assert_eq!(MP0_C2PMSG_33, 0x58184);
        assert_eq!((DC_SMU_MSG, DC_SMU_ARG, DC_SMU_RESP), (0x58A0C, 0x58A4C, 0x58A6C));
        assert_eq!(RCC_CONFIG_MEMSIZE, 0x378C);
        assert_eq!(RCC_CONFIG_APER_SIZE, 0x379C);
        assert!(NBIO_RSMU_ALIAS.contains(&RSMU_INDEX) && NBIO_RSMU_ALIAS.contains(&RSMU_DATA));
    }

    #[test]
    fn r22_wrong_base_mix_differs() {
        // mp_13_0_5 idx0 paired with the Phoenix list lands on 0x4000 + offset: valid, wrong, silent.
        assert_ne!(DC_SMU_RESP, (0x4000 + 0x29B) * 4);
        assert_eq!(DC_SMU_RESP, (0x16000 + 0x29B) * 4);
    }
}
