//! The PCIe rings in host memory (JS js/pci.mjs `pciAlloc`, the JS `d.pci`):
//! TX buffer descriptor rings with their buffers, RX rings with buffers and
//! their descriptors filled. The lengths are parameters; `RingConfig::js()` are
//! the JS driver's (and so the golden trace's), `RingConfig::linux()` Linux's.

use alloc::vec::Vec;

use crate::bus::{Bus, BusError};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::tables::regs::*;

/// Ring lengths per channel the 8852BE uses (enum rtw89_tx_channel; RXQ, RPQ)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingConfig {
    pub ach0: u32,
    pub ach1: u32,
    pub ach2: u32,
    pub ach3: u32,
    pub ch8: u32,
    pub ch9: u32,
    pub ch12: u32,
    pub rxq: u32,
    pub rpq: u32,
}

impl RingConfig {
    /// js/pci.mjs TX_LEN and RX_LEN ("ring sizes chosen small"), the golden trace's
    pub const fn js() -> RingConfig {
        RingConfig {
            ach0: 16,
            ach1: 4,
            ach2: 4,
            ach3: 4,
            ch8: 16,
            ch9: 4,
            ch12: 16,
            rxq: 64,
            rpq: 16,
        }
    }

    /// Linux's ring lengths: RTW89_PCI_TXBD_NUM_MAX and RTW89_PCI_RXBD_NUM_MAX (pci.h:1122)
    /// for every channel (rtw89_pci_alloc_tx_rings pci.c:3630, _rx_rings pci.c:3764)
    pub const fn linux() -> RingConfig {
        RingConfig {
            ach0: 256,
            ach1: 256,
            ach2: 256,
            ach3: 256,
            ch8: 256,
            ch9: 256,
            ch12: 256,
            rxq: 256,
            rpq: 256,
        }
    }
}

/// The longest ring the chip can index: its index fields are 12 bits
/// (TXBD_HOST_IDX_MASK, TXBD_HW_IDX_MASK, pci.h:558). A hardware limit, not a count limit.
pub const RING_LEN_MAX: u32 = 0xfff;

/// rtw89_bd_ram_table_single (pci.c:1711): start index, max and min of each TX
/// channel's BD RAM; `None` for a channel the 8852BE does not use
pub const fn bd_ram(ch: u8) -> Option<(u32, u32, u32)> {
    Some(match ch {
        0 => (0, 5, 2),
        1 => (5, 5, 2),
        2 => (10, 5, 2),
        3 => (15, 5, 2),
        8 => (20, 4, 1),
        9 => (24, 4, 1),
        12 => (28, 4, 1),
        _ => return None,
    })
}

/// One BD entry is 8 bytes (struct rtw89_pci_tx_bd_32, rtw89_pci_rx_bd_32)
const BD_SIZE: u32 = 8;
/// BD rings start at 128 bytes (the JS kit's choice; Linux's dma_alloc_coherent gives pages)
const BD_ALIGN: usize = 128;
/// A CH12 buffer: WD body (24) + up to 2020 bytes of firmware piece or H2C
pub const CMD_BUF: usize = 2048;
/// Other TX buffers: a WD page (128) + the frame
pub const TX_BUF: usize = 2048 + 128;
/// RX buffer sizes (RTW89_PCI_RX_BUF_SIZE is larger in Linux; the JS's)
pub const RXQ_BUF: u32 = 4096;
pub const RPQ_BUF: u32 = 2048;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxRing {
    pub name: &'static str,
    pub ch: u8,
    pub len: u32,
    pub wp: u32,
    pub rp: u32,
    /// bus address of the BD ring
    pub bd: u32,
    pub bufs: Vec<u32>,
    pub num: u32,
    pub idx: u32,
    pub bdram: u32,
    pub desa_l: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxRing {
    pub name: &'static str,
    pub len: u32,
    pub wp: u32,
    pub rp: u32,
    /// the host index write of the last poll did not reach the chip: the next poll repeats it
    pub idx_stale: bool,
    pub bd: u32,
    pub size: u32,
    pub bufs: Vec<u32>,
    pub num: u32,
    pub idx: u32,
    pub desa_l: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PciRings {
    pub tx: Vec<TxRing>,
    pub rx: Vec<RxRing>,
    /// every DMA region handed out for the rings (address, size), in order
    pub dma: Vec<(u32, usize)>,
    /// the PCIe capability's offset once found (JS `d.pci.expCap`, `pci::cfg::exp_cap`)
    pub exp_cap: Option<u16>,
}

impl PciRings {
    pub fn tx(&self, name: &str) -> Option<&TxRing> {
        self.tx.iter().find(|r| r.name == name)
    }
    pub fn rx(&self, name: &str) -> Option<&RxRing> {
        self.rx.iter().find(|r| r.name == name)
    }
    /// The ring by name, or an error naming it (never a panic)
    pub fn tx_mut(&mut self, name: &str) -> Rtw89Result<&mut TxRing> {
        self.tx
            .iter_mut()
            .find(|r| r.name == name)
            .ok_or(Rtw89Error::Invalid {
                what: "no such ring",
                value: 0,
            })
    }
    pub fn rx_mut(&mut self, name: &str) -> Rtw89Result<&mut RxRing> {
        self.rx
            .iter_mut()
            .find(|r| r.name == name)
            .ok_or(Rtw89Error::Invalid {
                what: "no such ring",
                value: 0,
            })
    }

    fn alloc<B: Bus + ?Sized>(
        &mut self,
        bus: &mut B,
        size: usize,
        align: usize,
    ) -> Rtw89Result<u32> {
        let a = bus.dma_alloc(size, align)?;
        self.dma.push((a, size));
        Ok(a)
    }

    /// Gives every region back, newest first, even when one fails (the first error is returned).
    /// Only after bus master is off: the chip must not reach them any more.
    pub fn free<B: Bus + ?Sized>(&mut self, bus: &mut B) -> Rtw89Result<()> {
        let mut first = None;
        while let Some((a, size)) = self.dma.pop() {
            if let Err(e) = bus.dma_free(a, size) {
                first.get_or_insert(e);
            }
        }
        first.map_or(Ok(()), |e| Err(e.into()))
    }
}

fn bytes(len: u32, each: u32) -> Rtw89Result<usize> {
    len.checked_mul(each)
        .map(|n| n as usize)
        .ok_or(Rtw89Error::Bus(BusError::DmaExhausted {
            size: usize::MAX,
            align: BD_ALIGN,
        }))
}

fn offset(base: u32, by: u32) -> Rtw89Result<u32> {
    base.checked_add(by)
        .ok_or(Rtw89Error::BadDmaAddress { addr: base })
}

// Linux: rtw89_pci_setup_resource (pci.c:3853) -> rtw89_pci_alloc_trx_rings (pci.c:3821):
// rtw89_pci_alloc_tx_rings (pci.c:3630) / _tx_ring (pci.c:3590), then
// rtw89_pci_alloc_rx_rings (pci.c:3764) / _rx_ring (pci.c:3692) with
// rtw89_pci_init_rx_bd (pci.c:3512) per buffer: buf_size, opt (DMA high bits 0), dma.
// The rxbd_info word of each buffer is cleared, so a stale one never looks new.
// Allocation order is the JS driver's: a ring's BDs, then its buffers. On an
// error the regions handed out so far are given back (rtw89_pci_alloc_trx_rings' unwind).
pub fn pci_alloc<B: Bus + ?Sized>(bus: &mut B, cfg: &RingConfig) -> Rtw89Result<PciRings> {
    check_lengths(cfg)?;
    let mut rings = PciRings::default();
    match fill(bus, cfg, &mut rings) {
        Ok(()) => Ok(rings),
        Err(e) => {
            let _ = rings.free(bus);
            Err(e)
        }
    }
}

/// Every ring needs at least one entry and an index the chip can hold; checked
/// before any DMA memory is asked for. Lengths too big for the DMA memory fail there.
fn check_lengths(cfg: &RingConfig) -> Rtw89Result<()> {
    let all = [
        cfg.ach0, cfg.ach1, cfg.ach2, cfg.ach3, cfg.ch8, cfg.ch9, cfg.ch12, cfg.rxq, cfg.rpq,
    ];
    match all.iter().find(|&&n| n == 0 || n > RING_LEN_MAX) {
        Some(&n) if n == 0 => Err(Rtw89Error::Invalid {
            what: "ring length 0",
            value: n,
        }),
        Some(&n) => Err(Rtw89Error::Invalid {
            what: "ring length past the chip's 12-bit index",
            value: n,
        }),
        None => Ok(()),
    }
}

fn fill<B: Bus + ?Sized>(bus: &mut B, cfg: &RingConfig, rings: &mut PciRings) -> Rtw89Result<()> {
    let tx_defs: [(&'static str, u8, u32, u32, u32, u32, u32); 7] = [
        (
            "ACH0",
            0,
            cfg.ach0,
            R_AX_ACH0_TXBD_NUM,
            R_AX_ACH0_TXBD_IDX,
            R_AX_ACH0_BDRAM_CTRL,
            R_AX_ACH0_TXBD_DESA_L,
        ),
        (
            "ACH1",
            1,
            cfg.ach1,
            R_AX_ACH1_TXBD_NUM,
            R_AX_ACH1_TXBD_IDX,
            R_AX_ACH1_BDRAM_CTRL,
            R_AX_ACH1_TXBD_DESA_L,
        ),
        (
            "ACH2",
            2,
            cfg.ach2,
            R_AX_ACH2_TXBD_NUM,
            R_AX_ACH2_TXBD_IDX,
            R_AX_ACH2_BDRAM_CTRL,
            R_AX_ACH2_TXBD_DESA_L,
        ),
        (
            "ACH3",
            3,
            cfg.ach3,
            R_AX_ACH3_TXBD_NUM,
            R_AX_ACH3_TXBD_IDX,
            R_AX_ACH3_BDRAM_CTRL,
            R_AX_ACH3_TXBD_DESA_L,
        ),
        (
            "CH8",
            8,
            cfg.ch8,
            R_AX_CH8_TXBD_NUM,
            R_AX_CH8_TXBD_IDX,
            R_AX_CH8_BDRAM_CTRL,
            R_AX_CH8_TXBD_DESA_L,
        ),
        (
            "CH9",
            9,
            cfg.ch9,
            R_AX_CH9_TXBD_NUM,
            R_AX_CH9_TXBD_IDX,
            R_AX_CH9_BDRAM_CTRL,
            R_AX_CH9_TXBD_DESA_L,
        ),
        (
            "CH12",
            12,
            cfg.ch12,
            R_AX_CH12_TXBD_NUM,
            R_AX_CH12_TXBD_IDX,
            R_AX_CH12_BDRAM_CTRL,
            R_AX_CH12_TXBD_DESA_L,
        ),
    ];
    for (name, ch, len, num, idx, bdram, desa_l) in tx_defs {
        let bd = rings.alloc(bus, bytes(len, BD_SIZE)?, BD_ALIGN)?;
        let (size, align) = if name == "CH12" {
            (CMD_BUF, 8)
        } else {
            (TX_BUF, 128)
        };
        let mut bufs = Vec::new();
        for _ in 0..len {
            bufs.push(rings.alloc(bus, size, align)?);
        }
        rings.tx.push(TxRing {
            name,
            ch,
            len,
            wp: 0,
            rp: 0,
            bd,
            bufs,
            num,
            idx,
            bdram,
            desa_l,
        });
    }
    let rx_defs: [(&'static str, u32, u32, u32, u32, u32); 2] = [
        (
            "RXQ",
            cfg.rxq,
            RXQ_BUF,
            R_AX_RXQ_RXBD_NUM,
            R_AX_RXQ_RXBD_IDX,
            R_AX_RXQ_RXBD_DESA_L,
        ),
        (
            "RPQ",
            cfg.rpq,
            RPQ_BUF,
            R_AX_RPQ_RXBD_NUM,
            R_AX_RPQ_RXBD_IDX,
            R_AX_RPQ_RXBD_DESA_L,
        ),
    ];
    for (name, len, size, num, idx, desa_l) in rx_defs {
        let bd = rings.alloc(bus, bytes(len, BD_SIZE)?, BD_ALIGN)?;
        let mut bufs = Vec::new();
        for _ in 0..len {
            bufs.push(rings.alloc(bus, size as usize, 8)?);
        }
        for (i, buf) in bufs.iter().enumerate() {
            let at = offset(bd, i as u32 * BD_SIZE)?;
            bus.mw32(at, size & 0xffff)?;
            bus.mw32(offset(at, 4)?, *buf)?;
            bus.mw32(*buf, 0)?;
        }
        rings.rx.push(RxRing {
            name,
            len,
            wp: 0,
            rp: 0,
            idx_stale: false,
            bd,
            size,
            bufs,
            num,
            idx,
            desa_l,
        });
    }
    bus.flush()?;
    Ok(())
}
