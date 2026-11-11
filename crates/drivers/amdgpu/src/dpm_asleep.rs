//! GX-9b: one step of the job half on a block whose GFXOFF could not be taken back. Memory and doorbells work (fences and
//! read pointers are memory, a doorbell wakes the block), so a job still ends or is seen hung; every register answers
//! nothing, so nothing reads or writes a block that may be powered down. A rebuild is registers through and through: it
//! is refused, and the hang path counts the device as lost, which is bounded.

use crate::gpucore::mes::MesAddrs;
use crate::gpucore::psp::Hw;
use crate::gpucore::vmring::{VmAddrs, VmInputs};
use crate::hw::chain::{ChainOwner, Rebuilt};
use alloc::string::String;

pub(crate) struct Asleep<'a>(pub &'a mut dyn ChainOwner);

impl ChainOwner for Asleep<'_> {
    fn io(&mut self) -> &mut dyn Hw {
        self
    }
    fn clear_fault(&mut self, _a: &VmAddrs, _us: u64) -> Option<u32> {
        None
    }
    fn rebuild_compute(&mut self, _inp: &VmInputs) -> Rebuilt {
        refused()
    }
    fn rebuild_gfx(&mut self, _inp: &VmInputs) -> Rebuilt {
        refused()
    }
    fn restart_sdma(&mut self, _inp: &VmInputs) -> Rebuilt {
        refused()
    }
    fn kill_waves(&mut self, _a: &MesAddrs, _vmid: u8) -> bool {
        false
    }
    fn display_writes(&mut self) -> u64 {
        self.0.display_writes()
    }
}

fn refused() -> Rebuilt {
    Rebuilt { queue: Err("GC sleeps and PMFW did not give it back"), detail: String::from("no register was touched"), stranded: false }
}

impl Hw for Asleep<'_> {
    fn reg_read(&mut self, _byte: u32) -> Option<u32> {
        None
    }
    fn reg_write(&mut self, _byte: u32, _value: u32) -> bool {
        false
    }
    fn mem_read(&mut self, mc: u64, buf: &mut [u8]) -> bool {
        self.0.io().mem_read(mc, buf)
    }
    fn mem_write(&mut self, mc: u64, data: &[u8]) -> bool {
        self.0.io().mem_write(mc, data)
    }
    fn now_us(&mut self) -> u64 {
        self.0.io().now_us()
    }
    fn delay_us(&mut self, us: u64) {
        self.0.io().delay_us(us);
    }
    fn flush_hdp(&mut self) {
        self.0.io().flush_hdp();
    }
    fn invalidate_hdp(&mut self) {
        self.0.io().invalidate_hdp();
    }
    fn mem_write64(&mut self, mc: u64, value: u64) -> bool {
        self.0.io().mem_write64(mc, value)
    }
    fn store_barrier(&mut self) {
        self.0.io().store_barrier();
    }
    fn doorbell_write64(&mut self, index_dw: u32, value: u64) -> bool {
        self.0.io().doorbell_write64(index_dw, value)
    }
}
