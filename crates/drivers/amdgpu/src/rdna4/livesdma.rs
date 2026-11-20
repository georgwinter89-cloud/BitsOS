//! Wave 43: the kernel glue of the copy engines: the Reload hook that stops them (`sdmaoff::stop`, Linux's
//! `sdma_v7_0_hw_fini`). A running engine reads its ring and writes its slots in system pages of the kernel that
//! started it, so the hook is registered BEFORE the stage `sdma` writes its first register (`livestage::tool`),
//! whether that stage then passes or stops with an MCU unhalted or a ring already on. Registered after the IH's
//! hook, it runs before it (newest first): the engines are stopped while the IH still stands, as Linux's
//! `hw_fini` order has them. Until the stage says it set out to write the block (`sdmaoff::Written`, handed to
//! the session by `livestage::tool`) the hook reads and writes nothing: the GC block may be without power. It
//! takes no lock, allocates nothing and logs nothing.

use super::ihoff::OffIo;
use super::sdmaoff::{self, OffRegs, Written};
use super::sdmaregs::SdmaRegs;
use crate::devs::{Device, Key};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hal::mmio::IoMem;
use hal::paging::CacheType;
use ksync::Mutex;

struct SdmaQuiesce {
    bar5: IoMem,
    regs: OffRegs,
    written: Written,
}

struct QuiesceIo<'a>(&'a IoMem);

impl OffIo for QuiesceIo<'_> {
    fn read(&mut self, byte: u32) -> Option<u32> {
        self.0.read32(byte as usize).ok()
    }
    fn write(&mut self, byte: u32, v: u32) -> bool {
        self.0.write32(byte as usize, v).is_ok()
    }
    fn delay_us(&mut self, us: u64) {
        time::delay_us(us)
    }
}

impl devmodel::Quiesce for SdmaQuiesce {
    fn name(&self) -> &str {
        "amdgpu.rdna4.sdma"
    }
    fn quiesce(&self) {
        let _ = sdmaoff::stop(&mut QuiesceIo(&self.bar5), &self.regs, &self.written);
    }
}

/// Cards whose hook is registered, each with the word its hook waits for.
static ARMED: Mutex<Vec<(Key, Written)>> = Mutex::named("amdgpu.rdna4.sdma_quiesce", Vec::new());

/// From now on a Reload stops the card's copy engines once the stage has set the word this hands back.
pub fn arm_quiesce(dev: &Device) -> Result<Written, String> {
    let card = super::live::card_now(dev).ok_or("the card is not attached")?;
    let att = card.state.as_ref().map_err(|_| "the attach stopped")?;
    let bar5 = card.facts.bar5.ok_or("BAR5 missing")?;
    let regs = SdmaRegs::from(&att.discovery).and_then(|r| OffRegs::of(&r)).ok_or("the table does not name the engines' registers")?;
    let mut armed = ARMED.lock();
    if let Some((_, written)) = armed.iter().find(|(k, _)| *k == dev.key) {
        return Ok(written.clone());
    }
    let bar5 = hal::mmio::map_device(bar5.phys, bar5.bytes as usize, CacheType::Uncached).map_err(|e| format!("BAR5: {e:?}"))?;
    let written = Written::default();
    devmodel::register_quiesce(Arc::new(SdmaQuiesce { bar5, regs, written: written.clone() }));
    armed.push((dev.key, written.clone()));
    Ok(written)
}
