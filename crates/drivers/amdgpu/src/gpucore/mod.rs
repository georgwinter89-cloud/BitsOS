//! GX-6m: the common core of the render driver (GX-D1/D2), beside the display side that exists (`dcn`, `attach`,
//! `live`, ...; those keep their paths this wave, SN-7 edits `live.rs`). What lives here is shared by every IP
//! module and hard-wired to no GPU:
//! - `hwid`: the hardware IDs the discovery table uses;
//! - `disc`: the discovery binary parser (versions, instances, register bases, GC/harvest/VCN/MALL tables);
//! - `vram`: reading the binary out of graphics memory (index/data window, or an APU's carve-out);
//! - `fw`: firmware file names from IP versions; GX-7a: `fwload` reads them from the stick, `ucode` checks their
//!   headers and makes the PSP's load list, `psp` is the PSP command protocol and bring-up, `pspfake` the fake
//!   PSP (registers, GPU memory, clock) it runs against in this wave.
//! - GX-7b: `bringup` is the bring-up register owner (every write read back and kept in its own large log; its
//!   live form writes nothing yet), `classify` reads what state the GPU is in at our start.
//! - GX-7c: the fakes grown to the real chip (`pspfake` memory-aware, `smufake` PMFW, `gcfake` GFXOFF/IMU/RLC,
//!   `fakechip` the 780M table and the start states), the scanout watcher (`watch`, GX-D9 as a check), Linux's
//!   register trace (`regtrace`) and the phase-by-phase diff against it (`regdiff`).
//! - GX-7d: `pspbringup` is the PSP run the desk starts (M1, M2). GX-7e: `smu` is the driver's PMFW mailbox
//!   (pre-poll, the allowed messages only), `gfxup` the GC power-up (M3: EnableGfxImu without waiting, IMU flags,
//!   CP_STAT, the RLC bootload) and the mode-2 recovery.
//! - GX-7h: `gmc` is the GART and VMID 0 on both hubs (MMHUB only where the display does not care), `carvetable`
//!   the page-table storage in the carve-out, `hubfake` the hubs in the fakes (flush acks, translation, faults).
//! - GX-7i: `sdma` is SDMA 0's ring and rungs (M4), `sdmafake` the engine in the fakes. GX-7j: `gfxring` is the
//!   GFX tail and GFX ring 0 driven by the CP directly (M5), `gfxfake` the CP in the fakes.
//! - GX-7m: `vmring` is the test VM in the rings (non-zero VMID, in-ring flush, PASID entry, an intended fault).
//!   `vmproc` runs the same VM through a MES process queue (ADD_QUEUE with the page table, a VMID MES picks).
//!   GX-7m3: `vmrecover` brings back a queue stalled at its intended fault (CPC has no way to drop it).
//!
//! Later parts of the core (GX-7..GX-9): the MMIO owner for the whole BAR (today `dcn::DcnRegs`), rings, fences,
//! GPU VM, the uAPI. The IP modules (`crate::ipmod`) are chosen from `disc`'s table.

pub mod bringup;
pub mod carvetable;
pub mod classify;
pub mod disc;
pub mod fakechip;
pub mod fw;
pub mod fwload;
pub mod gcfake;
pub mod gfxfake;
pub mod gfxring;
pub mod gfxup;
pub mod gmc;
pub mod handoff;
pub mod hubfake;
pub mod hwid;
pub mod ih;
pub mod ihfake;
pub mod mes;
pub mod mesfake;
pub mod mesq;
pub mod psp;
pub mod pspbringup;
pub mod pspfake;
pub mod regdiff;
pub mod regtrace;
pub mod sdma;
pub mod sdmafake;
pub mod smu;
pub mod smufake;
pub mod ucode;
pub mod vmproc;
pub mod vmrecover;
pub mod vmring;
pub mod vram;
pub mod watch;
