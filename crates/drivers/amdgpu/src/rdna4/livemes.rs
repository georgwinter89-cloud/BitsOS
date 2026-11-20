//! Wave 44, item ME: the kernel glue of MES for a Reload. The hook (`mesoff::halt`, Linux's
//! `mes_v12_0_enable(adev, false)`) is registered before the stage `mes` writes its first register
//! (`livestage::run_one`), whether the stage then passes or stops with a pipe started. Registered after GFXHUB's
//! hook and before the copy engines', it runs between them (newest first): the engines stopped, MES halted, then
//! GFXHUB off and the IH off, Linux's reverse order of blocks (sdma before gfx's fini, whose `kiq_hw_fini` halts
//! MES before GFXHUB's switch-off). Until the stage sets the word the hook reads and writes nothing. No lock, no
//! allocation, no sleep.

use super::liveih::QuiesceIo;
use super::mesoff::{self, Written};
use super::nbregs::NbRegs;
use crate::devs::{Device, Key};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hal::mmio::IoMem;
use hal::paging::CacheType;
use ksync::Mutex;

struct MesQuiesce {
    bar5: IoMem,
    n: NbRegs,
    written: Written,
}

impl devmodel::Quiesce for MesQuiesce {
    fn name(&self) -> &str {
        "amdgpu.rdna4.mes"
    }
    fn quiesce(&self) {
        let _ = mesoff::halt(&mut QuiesceIo(&self.bar5), &self.n, &self.written);
    }
}

/// Cards whose hook is registered, each with the word its hook waits for.
static ARMED: Mutex<Vec<(Key, Written)>> = Mutex::named("amdgpu.rdna4.mes_quiesce", Vec::new());

/// From now on a Reload halts the card's MES once the stage has set the word this hands back.
pub fn arm_quiesce(dev: &Device) -> Result<Written, String> {
    let card = super::live::card_now(dev).ok_or("the card is not attached")?;
    let att = card.state.as_ref().map_err(|_| "the attach stopped")?;
    let bar5 = card.facts.bar5.ok_or("BAR5 missing")?;
    let n = super::regs14::Regs14::from(&att.discovery).and_then(|r| NbRegs::from(&att.discovery, &r)).ok_or("the table does not name the registers")?;
    let mut armed = ARMED.lock();
    if let Some((_, written)) = armed.iter().find(|(k, _)| *k == dev.key) {
        return Ok(written.clone());
    }
    let bar5 = hal::mmio::map_device(bar5.phys, bar5.bytes as usize, CacheType::Uncached).map_err(|e| format!("BAR5: {e:?}"))?;
    let written = Written::default();
    devmodel::register_quiesce(Arc::new(MesQuiesce { bar5, n, written: written.clone() }));
    armed.push((dev.key, written.clone()));
    Ok(written)
}
