//! Wave 44, item RG: the kernel glue of the rings' interrupt enables for a Reload: the hook that clears them as
//! Linux's suspend does (`ringoff::off`). Registered before `gcring` writes its first register (`livestage::tool`),
//! after MES's hook, so it runs before MES's and the command processor's (newest first). Until the stage says it
//! set out to write (`sdmaoff::Written`, handed to the session by `livestage::tool`) the hook reads and writes
//! nothing. It takes no lock, allocates nothing and logs nothing.

use super::liveih::QuiesceIo;
use super::nbregs::NbRegs;
use super::ringoff::{self, OffRegs};
use super::sdmaoff::Written;
use crate::devs::{Device, Key};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hal::mmio::IoMem;
use hal::paging::CacheType;
use ksync::Mutex;

struct RingQuiesce {
    bar5: IoMem,
    regs: OffRegs,
    written: Written,
}

impl devmodel::Quiesce for RingQuiesce {
    fn name(&self) -> &str {
        "amdgpu.rdna4.rings"
    }
    fn quiesce(&self) {
        let _ = ringoff::off(&mut QuiesceIo(&self.bar5), &self.regs, &self.written);
    }
}

/// Cards whose hook is registered, each with the word its hook waits for.
static ARMED: Mutex<Vec<(Key, Written)>> = Mutex::named("amdgpu.rdna4.ring_quiesce", Vec::new());

/// From now on a Reload clears the rings' interrupt enables once the stage has set the word this hands back.
pub fn arm_quiesce(dev: &Device) -> Result<Written, String> {
    let card = super::live::card_now(dev).ok_or("the card is not attached")?;
    let att = card.state.as_ref().map_err(|_| "the attach stopped")?;
    let bar5 = card.facts.bar5.ok_or("BAR5 missing")?;
    let regs = super::regs14::Regs14::from(&att.discovery).and_then(|r| NbRegs::from(&att.discovery, &r)).and_then(|n| OffRegs::of(&n)).ok_or("the table does not name the rings' interrupt controls")?;
    let mut armed = ARMED.lock();
    if let Some((_, written)) = armed.iter().find(|(k, _)| *k == dev.key) {
        return Ok(written.clone());
    }
    let bar5 = hal::mmio::map_device(bar5.phys, bar5.bytes as usize, CacheType::Uncached).map_err(|e| format!("BAR5: {e:?}"))?;
    let written = Written::default();
    devmodel::register_quiesce(Arc::new(RingQuiesce { bar5, regs, written: written.clone() }));
    armed.push((dev.key, written.clone()));
    Ok(written)
}
