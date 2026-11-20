//! Wave 44, item CP: the kernel glue of the command processor for a Reload: the hook that halts it as Linux's fini
//! does (`cpoff::halt`). An unhalted engine fetches from queues of the kernel that started it, so the hook is
//! registered BEFORE the stage `gccp` writes its first register (`livestage::tool`), whether the stage then passes
//! or stops with an engine unhalted. Registered after GFXHUB's hook and before MES's (`livemes`, at `mes`), it
//! runs between them (newest first): MES halted first, then the engines while their hub still stands, as Linux's
//! `hw_fini` has it (`amdgpu_mes_kiq_hw_fini` gfx_v12_0.c:3902, `cp_enable(false)` :3914, `gart_disable` :3917;
//! the host test `rdna4ringreload` holds the order). Until the stage says it
//! set out to write the block (`sdmaoff::Written`, handed to the session by `livestage::tool`) the hook reads and
//! writes nothing. It takes no lock, allocates nothing and logs nothing.

use super::cpoff::{self, OffRegs};
use super::liveih::QuiesceIo;
use super::nbregs::NbRegs;
use super::sdmaoff::Written;
use crate::devs::{Device, Key};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hal::mmio::IoMem;
use hal::paging::CacheType;
use ksync::Mutex;

struct CpQuiesce {
    bar5: IoMem,
    regs: OffRegs,
    written: Written,
}

impl devmodel::Quiesce for CpQuiesce {
    fn name(&self) -> &str {
        "amdgpu.rdna4.cp"
    }
    fn quiesce(&self) {
        let _ = cpoff::halt(&mut QuiesceIo(&self.bar5), &self.regs, &self.written);
    }
}

/// Cards whose hook is registered, each with the word its hook waits for.
static ARMED: Mutex<Vec<(Key, Written)>> = Mutex::named("amdgpu.rdna4.cp_quiesce", Vec::new());

/// From now on a Reload halts the card's command processor once the stage has set the word this hands back.
pub fn arm_quiesce(dev: &Device) -> Result<Written, String> {
    let card = super::live::card_now(dev).ok_or("the card is not attached")?;
    let att = card.state.as_ref().map_err(|_| "the attach stopped")?;
    let bar5 = card.facts.bar5.ok_or("BAR5 missing")?;
    let regs = super::regs14::Regs14::from(&att.discovery).and_then(|r| NbRegs::from(&att.discovery, &r)).and_then(|n| OffRegs::of(&n)).ok_or("the table does not name the command processor's registers")?;
    let mut armed = ARMED.lock();
    if let Some((_, written)) = armed.iter().find(|(k, _)| *k == dev.key) {
        return Ok(written.clone());
    }
    let bar5 = hal::mmio::map_device(bar5.phys, bar5.bytes as usize, CacheType::Uncached).map_err(|e| format!("BAR5: {e:?}"))?;
    let written = Written::default();
    devmodel::register_quiesce(Arc::new(CpQuiesce { bar5, regs, written: written.clone() }));
    armed.push((dev.key, written.clone()));
    Ok(written)
}
