//! Wave 42 (piece 11): the IH rings as the CPU reads them, Linux's `amdgpu_ih_process` (amdgpu_ih.c:209-248) with
//! `ih_v7_0_get_wptr` (ih_v7_0.c:453-498), `ih_v7_0_set_rptr` (:525-550) and `amdgpu_irq_dispatch`
//! (amdgpu_irq.c:468-533): the write pointer from its write-back with the overflow check, entries of eight dwords
//! up to it, the read pointer back into its slot and through the ring's doorbell, the write pointer once more.
//!
//! Linux drops an entry nobody registered for with a debug line; here each is counted per (client, source) and
//! logged once per pair, so the hardware round sees what arrived. The entries with a taker: the IH's own (client
//! 0, source 0), ring 1's write pointer (`ih_v7_0_self_irq` :552-566), after which ring 1 is read; and the two
//! hubs' page faults (`ihfault`, wave 43: client VMC source 0 and client GFX source 0, as `gmc_v12_0_sw_init`
//! registers them, gmc_v12_0.c:865-893).
//!
//! Wave 43: a block takes a source's entries by registering for it (`Reader::register`, Linux's
//! `amdgpu_irq_add_id`): they are then counted for it per ring id (`taken`) instead of listed as unhandled. SDMA
//! is the first (its trap: client 10, source 49, the engine in the ring id's high nibble). A handler that refuses
//! an entry, as Linux's SDMA trap handler does one that names an engine the card does not have, has it counted
//! as `refused` (Linux: the handler's -EINVAL, logged by `amdgpu_irq_dispatch`), not as taken.
//!
//! Wave 44: the card's thermal sources go to the thermal taker (`thermal`): THM sources 0 and 1 and MP1's
//! source 0xFE, the three `smu_v14_0_register_irq_handler` registers (../pm/swsmu/smu14/smu_v14_0.c:891-922). A
//! THM or MP1 entry of another source is counted and logged at Error once per pair, never only listed.
//!
//! Stricter than Linux: a pass is bounded (twice the ring), a write pointer off the 32-byte grid is cut down, and
//! a ring whose memory or registers do not read is left alone and counted.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use rdna4_core::hub41;
use rdna4_core::thm14;
use rdna4_core::ih7::{self, Iv, Ring, f};

/// Where the rings lie: one piece of system memory, ring 0, ring 1, then the write-back page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IhPlace {
    /// The piece's system address.
    pub sys: u64,
    pub ring_mc: [u64; 2],
    pub wb_mc: u64,
}

impl IhPlace {
    pub const WB_BYTES: u64 = 4096;

    fn index(ring: Ring) -> usize {
        match ring {
            Ring::Ring0 => 0,
            Ring::Ring1 => 1,
        }
    }
    /// Byte offsets into the piece.
    pub fn ring_at(ring: Ring) -> u64 {
        Self::index(ring) as u64 * u64::from(ih7::RING_BYTES)
    }
    /// A ring's two slots of the write-back page: its write pointer (the card writes ring 0's, the self-interrupt's
    /// handler ring 1's) and its read pointer (Linux's `rptr_cpu`).
    pub fn wptr_at(ring: Ring) -> u64 {
        2 * u64::from(ih7::RING_BYTES) + Self::index(ring) as u64 * 8
    }
    pub fn rptr_at(ring: Ring) -> u64 {
        Self::wptr_at(ring) + 4
    }
    /// The MC address the card writes a ring's write pointer to (`IH_RB_WPTR_ADDR`, ring 0 only).
    pub fn wptr_mc(&self, ring: Ring) -> u64 {
        self.wb_mc + Self::index(ring) as u64 * 8
    }
}

/// What the reader reaches: the rings' memory, two registers per ring, the two doorbells, and for a page fault
/// the faulting hub's status (read) and its fault control (the clear bit, nothing else).
pub trait IhIo {
    /// A dword at byte `off` of the rings' piece of system memory.
    fn mem32(&mut self, off: u64) -> Option<u32>;
    fn set_mem32(&mut self, off: u64, v: u32) -> bool;
    /// IH_RB_WPTR and IH_RB_CNTL of a ring.
    fn wptr_reg(&mut self, ring: Ring) -> Option<u32>;
    fn cntl(&mut self, ring: Ring) -> Option<u32>;
    fn set_cntl(&mut self, ring: Ring, v: u32) -> bool;
    /// `WDOORBELL32(ih->doorbell_index, ih->rptr)`.
    fn doorbell(&mut self, ring: Ring, rptr: u32) -> bool;
    /// A hub's L2_PROTECTION_FAULT_STATUS_LO32, read.
    fn fault_status(&mut self, hub: hub41::Kind) -> Option<u32>;
    /// A hub's L2_PROTECTION_FAULT_CNTL read and written back with its clear bit (`hub41::fault_clear`:
    /// Linux's `WREG32_P(hub->vm_l2_pro_fault_cntl, 1, ~1)`, gmc_v12_0.c:130).
    fn fault_clear(&mut self, hub: hub41::Kind) -> bool;
    /// The clock, for the rate limit of a fault's lines and the thermal check's delay.
    fn now_us(&mut self) -> u64;
    /// The card for the thermal taker's register part (`thermalhw`, each access through its gate): `None` for a
    /// reader that does not reach the card's thermal and SMU registers (a stage's own); the taker then keeps
    /// that part for the next reader that does.
    fn thermal_hw(&mut self) -> Option<&mut dyn crate::gpucore::psp::Hw> {
        None
    }
}

/// What one call read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pass {
    pub entries: u32,
    /// The bound ended the pass before the ring was empty.
    pub more: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reader {
    rptr: [u32; 2],
    pub entries: [u64; 2],
    pub overflows: u64,
    /// A register or the ring's memory did not read: the pass ended there.
    pub bad_reads: u64,
    /// Entries with no handler, per (client, source).
    pub unclaimed: BTreeMap<(u8, u8), u64>,
    /// Self-interrupts handled (ring 1's write pointer).
    pub self_irqs: u64,
    /// The two hubs' page faults (`ihfault`).
    pub faults: super::ihfault::Faults,
    /// Sources a block registered for, with its name, and their entries per (client, source, ring id).
    takers: BTreeMap<(u8, u8), &'static str>,
    taken: BTreeMap<(u8, u8, u8), u64>,
    /// Entries of a registered source that its handler refused, per (client, source, ring id).
    refused: BTreeMap<(u8, u8, u8), u64>,
    /// Wave 44: the card's thermal interrupts and the software CTF check (`thermal`).
    pub thermal: super::thermal::Thermal,
}

impl Reader {
    pub fn rptr(&self, ring: Ring) -> u32 {
        self.rptr[IhPlace::index(ring)]
    }

    /// From now on the entries of this source are `who`'s: counted per ring id, not listed as unhandled.
    pub fn register(&mut self, client: u8, src: u8, who: &'static str) {
        self.takers.insert((client, src), who);
    }

    /// Entries of a registered source that arrived with this ring id.
    pub fn taken(&self, client: u8, src: u8, ring_id: u8) -> u64 {
        self.taken.get(&(client, src, ring_id)).copied().unwrap_or(0)
    }

    /// Every registered source's entries, per (client, source, ring id), with the taker's name.
    pub fn taken_all(&self) -> impl Iterator<Item = (&'static str, (u8, u8, u8), u64)> + '_ {
        self.taken.iter().map(|(k, n)| (self.takers.get(&(k.0, k.1)).copied().unwrap_or("?"), *k, *n))
    }

    /// Entries of a registered source its handler refused, per (client, source, ring id).
    pub fn refused_all(&self) -> impl Iterator<Item = ((u8, u8, u8), u64)> + '_ {
        self.refused.iter().map(|(k, n)| (*k, *n))
    }

    /// `ih_v7_0_get_wptr`.
    fn wptr(&mut self, io: &mut dyn IhIo, ring: Ring) -> Option<u32> {
        let mut w = io.mem32(IhPlace::wptr_at(ring))?;
        if f::RB_OVERFLOW.get(w) != 0 {
            w = io.wptr_reg(ring).filter(|v| *v != u32::MAX)?;
            if f::RB_OVERFLOW.get(w) != 0 {
                w = f::RB_OVERFLOW.put(w, 0);
                let next = ih7::after_overflow(w) & !(ih7::ENTRY_BYTES - 1);
                self.overflows += 1;
                klog::event!(Warn, "gx", "rdna4.ih.overflow", ring = IhPlace::index(ring), wptr = klog::Hex(u64::from(w)), rptr = klog::Hex(u64::from(self.rptr(ring))), next = klog::Hex(u64::from(next)); "IH ring buffer overflow");
                self.rptr[IhPlace::index(ring)] = next;
                // Clear, then unset "immediately so new overflows can be detected" (:480-487).
                let c = io.cntl(ring).filter(|v| *v != u32::MAX)?;
                if !io.set_cntl(ring, f::WPTR_OVERFLOW_CLEAR.put(c, 1)) || !io.set_cntl(ring, f::WPTR_OVERFLOW_CLEAR.put(c, 0)) {
                    return None;
                }
            }
        }
        Some(w & ih7::PTR_MASK & !(ih7::ENTRY_BYTES - 1))
    }

    fn entry(io: &mut dyn IhIo, ring: Ring, rptr: u32) -> Option<Iv> {
        let mut dw = [0u32; 8];
        for (i, d) in dw.iter_mut().enumerate() {
            *d = io.mem32(IhPlace::ring_at(ring) + u64::from(rptr) + 4 * i as u64)?;
        }
        Some(Iv::decode(&dw))
    }

    /// `amdgpu_irq_dispatch` for one entry; true when ring 1 has entries to read.
    fn dispatch(&mut self, io: &mut dyn IhIo, iv: &Iv) -> bool {
        if iv.client_id == ih7::CLIENT_IH && iv.src_id == 0 {
            self.self_irqs += 1;
            return iv.ring_id == 1 && io.set_mem32(IhPlace::wptr_at(Ring::Ring1), iv.src_data[0]);
        }
        if let Some(hub) = rdna4_core::fault12::taker(iv.client_id, iv.src_id) {
            self.faults.take(io, iv, hub);
            return false;
        }
        if let Some(src) = thm14::Source::of(iv.client_id, iv.src_id) {
            let now = io.now_us();
            self.thermal.take(io.thermal_hw(), src, iv, now);
            return false;
        }
        if [thm14::CLIENT_THM, thm14::CLIENT_MP1].contains(&iv.client_id) {
            self.thermal.foreign(iv);
        }
        if self.takers.contains_key(&(iv.client_id, iv.src_id)) {
            let key = (iv.client_id, iv.src_id, iv.ring_id);
            let wrong = if super::sdmaih::wrong_ring_id(iv) { Some("wrong sdma instance") } else if super::ringih::wrong_ring_id(iv) { Some("no kernel ring of the command processor") } else { None };
            if let Some(what) = wrong {
                let n = self.refused.entry(key).or_insert(0);
                *n += 1;
                if *n == 1 {
                    klog::event!(Warn, "gx", "rdna4.ih.refused", client = iv.client_id, source = iv.src_id, ring = iv.ring_id, what = what;
                        "IH: wrong ring_ID detected (counted from here on, logged this once)");
                }
                return false;
            }
            *self.taken.entry(key).or_insert(0) += 1;
            return false;
        }
        let n = self.unclaimed.entry((iv.client_id, iv.src_id)).or_insert(0);
        *n += 1;
        if *n == 1 {
            klog::event!(Info, "gx", "rdna4.ih.unclaimed", client = iv.client_id, source = iv.src_id, ring = iv.ring_id, data = klog::Hex(u64::from(iv.src_data[0]));
                "an interrupt source no block handles yet (counted from here on, logged this once)");
        }
        false
    }

    fn pass(&mut self, io: &mut dyn IhIo, ring: Ring) -> (Pass, bool) {
        let i = IhPlace::index(ring);
        let (mut pass, mut ring1) = (Pass::default(), false);
        let bound = 2 * ih7::RING_BYTES / ih7::ENTRY_BYTES;
        let Some(mut wptr) = self.wptr(io, ring) else {
            self.bad_reads += 1;
            return (pass, false);
        };
        loop {
            // Linux reads at most AMDGPU_IH_MAX_NUM_IVS - 1 entries between two updates of the read pointer.
            let mut count = ih7::MAX_NUM_IVS;
            while self.rptr[i] != wptr && count > 1 && pass.entries < bound {
                count -= 1;
                let Some(iv) = Self::entry(io, ring, self.rptr[i]) else {
                    self.bad_reads += 1;
                    return (pass, ring1);
                };
                ring1 |= self.dispatch(io, &iv);
                self.rptr[i] = (self.rptr[i] + ih7::ENTRY_BYTES) & ih7::PTR_MASK;
                self.entries[i] += 1;
                pass.entries += 1;
            }
            // ih_v7_0_set_rptr: the slot, then the doorbell (with RPTR_REARM this re-arms the message).
            if !io.set_mem32(IhPlace::rptr_at(ring), self.rptr[i]) || !io.doorbell(ring, self.rptr[i]) {
                self.bad_reads += 1;
                return (pass, ring1);
            }
            // "make sure wptr hasn't changed while processing".
            match self.wptr(io, ring) {
                Some(w) => wptr = w,
                None => {
                    self.bad_reads += 1;
                    return (pass, ring1);
                }
            }
            if wptr == self.rptr[i] {
                return (pass, ring1);
            }
            if pass.entries >= bound {
                pass.more = true;
                return (pass, ring1);
            }
        }
    }

    /// One look at the rings: ring 0, then ring 1 when a self-interrupt said it has entries (Linux's `ih1_work`).
    /// Acknowledges that waited for a reader reaching the card go first, a thermal check that came due last.
    pub fn process(&mut self, io: &mut dyn IhIo) -> [Pass; 2] {
        if !self.thermal.queue.is_empty()
            && let Some(hw) = io.thermal_hw()
        {
            self.thermal.replay(hw);
        }
        let (p0, ring1) = self.pass(io, Ring::Ring0);
        let p1 = if ring1 { self.pass(io, Ring::Ring1).0 } else { Pass::default() };
        if self.thermal.due_us.is_some() {
            let now = io.now_us();
            self.thermal.check(io.thermal_hw(), now);
        }
        [p0, p1]
    }

    pub fn describe(&self) -> String {
        let list: alloc::vec::Vec<String> = self.unclaimed.iter().map(|((c, s), n)| format!("client {c} source {s}: {n}")).collect();
        let named = |m: &BTreeMap<(u8, u8, u8), u64>| -> alloc::vec::Vec<String> { m.iter().map(|((c, s, r), n)| format!("{} client {c} source {s} ring id {r}: {n}", self.takers.get(&(*c, *s)).copied().unwrap_or("?"))).collect() };
        let (taken, refused) = (named(&self.taken), named(&self.refused));
        format!(
            "ring 0 {} entries (read pointer {:#x}), ring 1 {} entries (read pointer {:#x}), {} self-interrupts, {} overflows, {} failed reads; {}; taken: {}{}; no handler for: {}",
            self.entries[0],
            self.rptr[0],
            self.entries[1],
            self.rptr[1],
            self.self_irqs,
            self.overflows,
            self.bad_reads,
            self.faults.describe(),
            if taken.is_empty() { String::from("nothing") } else { taken.join(", ") },
            if refused.is_empty() { String::new() } else { format!("; refused by their handler (a ring id it does not know): {}", refused.join(", ")) },
            if list.is_empty() { String::from("nothing") } else { list.join(", ") }
        )
    }
}

/// The IH of one card in this boot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IhUp {
    pub place: IhPlace,
    pub reader: Reader,
    /// Where the MSI vector arrives.
    pub msi: String,
}

impl IhUp {
    pub fn new(place: IhPlace, msi: String) -> IhUp {
        IhUp { place, reader: Reader::default(), msi }
    }

    pub fn describe(&self) -> String {
        format!("on ({}; {})", self.msi, self.reader.describe())
    }
}
