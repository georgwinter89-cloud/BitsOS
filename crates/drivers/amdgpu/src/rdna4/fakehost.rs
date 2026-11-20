//! Wave 42: the machine's side of the model (`host::Host`): the DMAR survey's answer, the card's bus master and
//! the bridges above it, the PCI core's vector, system pages, and AtomicOp requester enable. Bus master before the
//! survey allowed it is refused and kept.

use super::fakemodel::Model;
use super::host::Host;
use alloc::string::String;
use rdna4_core::pte12;

impl Host for Model {
    fn dma_survey(&mut self) -> Result<(), String> {
        self.nb.survey_asked = true;
        self.nb.survey.map_err(String::from)
    }
    fn bus_master(&mut self) -> Option<bool> {
        Some(self.nb.bus_master)
    }
    fn bridges_forward(&mut self) -> Result<(), String> {
        self.nb.bridges.map_err(String::from)
    }
    fn atomics(&mut self) -> Result<String, String> {
        self.nb.atomics_path.map_err(String::from)?;
        if self.nb.atomics_req {
            return Ok(String::from("AtomicOps to the root port granted; requester enable was set already (the model's Device Control 2)"));
        }
        self.nb.atomics_req = true;
        self.nb.host_changes.push("AtomicOp requester enable");
        Ok(String::from("AtomicOps to the root port granted; requester enable set (the model's Device Control 2)"))
    }
    fn enable_dma(&mut self) -> Result<String, String> {
        if !self.nb.dma_ok() {
            self.refused.push(String::from("bus master before the DMAR survey allowed it"));
            return Err(String::from("refused by the model"));
        }
        self.nb.bus_master = true;
        self.nb.host_changes.push("bus master on");
        Ok(String::from("the model's command register"))
    }
    fn msi(&mut self) -> Result<String, String> {
        if self.nb.msi_refused || !self.nb.bus_master {
            return Err(String::from(if self.nb.msi_refused { "no vector" } else { "the function may not send" }));
        }
        self.nb.msi_on = true;
        self.nb.host_changes.push("MSI vector");
        Ok(String::from("MSI vector 0x41 on CPU 0 (the model's)"))
    }
    fn pages(&mut self, bytes: u64) -> Option<u64> {
        if self.nb.pages_refused {
            return None;
        }
        let at = self.nb.next_page;
        let end = at.checked_add(bytes.next_multiple_of(pte12::PAGE))?;
        self.nb.next_page = end;
        self.nb.sys_given.push(at..end);
        Some(at)
    }
    fn sys_read(&mut self, at: u64, buf: &mut [u8]) -> bool {
        let ok = self.nb.in_sys(at, buf.len() as u64);
        if ok {
            self.nb.sys_peek(at, buf);
        }
        ok
    }
    fn sys_write(&mut self, at: u64, data: &[u8]) -> bool {
        let ok = self.nb.in_sys(at, data.len() as u64);
        if ok {
            self.nb.sys_poke(at, data);
        }
        ok
    }
}
