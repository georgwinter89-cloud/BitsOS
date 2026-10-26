//! The PCIe host interface of the RTL8852BE (JS js/pci.mjs) and the MAC's DMA
//! pre-init (JS js/dmac.mjs), as Linux rtw89 pci.c and mac.c set them up for an
//! AX chip on PCIe (rtw8852be.c parameters). BitsOS polls here: no interrupt is
//! ever enabled. Every function keeps the JS driver's (= Linux's) order of
//! register accesses; where the JS deferred a check only to save a round trip
//! over MCP, the check sits at Linux's place (the trace of a good run is the same).

pub mod cfg;
pub mod dmac;
pub mod fwcmd;
pub mod init;
pub mod intr;
pub mod rings;
pub mod rx;

pub use cfg::{BasicCfg, basic_cfg, exp_cap};
pub use dmac::{DmacState, QtaMode, dle_init, dmac_pre_init, hci_dma_trx, hfc_init};
pub use fwcmd::{TxDescInfo, fill_txdesc, fwcmd, fwcmd_resync};
pub use init::{deinit, mac_pre_deinit, post_init, pre_init, reset};
pub use rings::{PciRings, RingConfig, RxRing, TxRing, pci_alloc};
pub use rx::{RxBuf, rx_poll};

#[cfg(test)]
mod tests;
