//! Wave 43, item HB: the kernel glue of the graphics block's hub for a Reload. From the first `gchub` call on a
//! Reload leaves GFXHUB as Linux's fini leaves it (`gchuboff::gart_disable`); the hook is registered before the
//! stage's first write, reads one register and writes nothing while the hub's context 0 is off. No lock, no
//! allocation, no sleep.

use super::gchuboff;
use super::liveih::QuiesceIo;
use super::nbregs::NbRegs;
use crate::devs::{Device, Key};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hal::mmio::IoMem;
use hal::paging::CacheType;
use ksync::Mutex;
use rdna4_core::hub41;

struct GcHubQuiesce {
    bar5: IoMem,
    n: NbRegs,
}

impl devmodel::Quiesce for GcHubQuiesce {
    fn name(&self) -> &str {
        "amdgpu.rdna4.gchub"
    }
    fn quiesce(&self) {
        let _ = gchuboff::gart_disable(&mut QuiesceIo(&self.bar5), &self.n, &hub41::GC);
    }
}

/// Cards whose hook is registered.
static ARMED: Mutex<Vec<Key>> = Mutex::named("amdgpu.rdna4.gchub_quiesce", Vec::new());

/// From now on a Reload switches the card's GFXHUB off if its context 0 is on, whether `gchub` passes or stops
/// with the hub already on.
pub fn arm_quiesce(dev: &Device) -> Result<(), String> {
    let card = super::live::card_now(dev).ok_or("the card is not attached")?;
    let att = card.state.as_ref().map_err(|_| "the attach stopped")?;
    let bar5 = card.facts.bar5.ok_or("BAR5 missing")?;
    let n = super::regs14::Regs14::from(&att.discovery).and_then(|r| NbRegs::from(&att.discovery, &r)).ok_or("the table does not name the registers")?;
    let mut armed = ARMED.lock();
    if !armed.contains(&dev.key) {
        let bar5 = hal::mmio::map_device(bar5.phys, bar5.bytes as usize, CacheType::Uncached).map_err(|e| alloc::format!("BAR5: {e:?}"))?;
        devmodel::register_quiesce(Arc::new(GcHubQuiesce { bar5, n }));
        armed.push(dev.key);
    }
    Ok(())
}
