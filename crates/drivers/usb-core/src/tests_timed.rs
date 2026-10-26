//! `Device::interrupt_pipe_at` on a host that does not track interrupt times: stamped on arrival.

use crate::desc::DeviceDescriptor;
use crate::device::Device;
use crate::host::{Data, HcSlot, Host, HubParams, PipeEvent, Sink, UsbError};
use crate::place::Place;
use crate::setup::Setup;
use crate::speed::Speed;
use crate::transfer::{Buffer, PipeOpts, Request, Urb};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::Mutex;
use std::sync::Mutex as StdMutex;
use std::vec::Vec;

#[derive(Default)]
struct Clocked {
    now: AtomicU64,
    sink: StdMutex<Option<Sink>>,
}

impl Host for Clocked {
    fn name(&self) -> &str {
        "clocked"
    }
    fn now_ns(&self) -> u64 {
        self.now.load(Ordering::Acquire)
    }
    fn sleep_ns(&self, _: u64) {}
    fn address(&self, _: &Place, _: u16, _: bool) -> Result<(HcSlot, u8), UsbError> {
        Err(UsbError::NotRunning)
    }
    fn set_address(&self, _: HcSlot) -> Result<u8, UsbError> {
        Err(UsbError::NotRunning)
    }
    fn set_mps0(&self, _: HcSlot, _: u16) -> Result<(), UsbError> {
        Ok(())
    }
    fn control(&self, _: HcSlot, _: Setup, _: Data<'_>) -> Result<usize, UsbError> {
        Err(UsbError::NotRunning)
    }
    fn configure(&self, _: HcSlot, _: &[crate::desc::Endpoint]) -> Result<(), UsbError> {
        Ok(())
    }
    fn set_hub(&self, _: HcSlot, _: HubParams) -> Result<(), UsbError> {
        Ok(())
    }
    fn interrupt_in(&self, _: HcSlot, _: u8, _: PipeOpts, sink: Sink) -> Result<(), UsbError> {
        *self.sink.lock().unwrap() = Some(sink);
        Ok(())
    }
    fn stop_pipe(&self, _: HcSlot, _: u8) -> Result<(), UsbError> {
        Ok(())
    }
    fn alloc_buffer(&self, _: usize) -> Result<Buffer, UsbError> {
        Err(UsbError::NoMemory)
    }
    fn submit(&self, _: HcSlot, _: u8, _: Request, _: Arc<Urb>) -> Result<(), UsbError> {
        Err(UsbError::NotRunning)
    }
    fn wait(&self, _: HcSlot, _: &Urb, _: u64) -> bool {
        true
    }
    fn cancel(&self, _: HcSlot, _: u8, _: &Arc<Urb>, _: UsbError) {}
    fn reset_endpoint(&self, _: HcSlot, _: u8) -> Result<(), UsbError> {
        Ok(())
    }
    fn abort_endpoint(&self, _: HcSlot, _: u8) -> Result<usize, UsbError> {
        Ok(0)
    }
    fn release(&self, _: HcSlot) {}
    fn kick(&self) {}
}

#[test]
fn a_host_without_interrupt_times_stamps_on_arrival() {
    let host = Arc::new(Clocked::default());
    let desc = DeviceDescriptor::parse(&[18, 1, 0, 2, 0, 0, 0, 8, 0x34, 0x12, 0x78, 0x56, 0, 1, 0, 0, 0, 1]).unwrap();
    let place = Place { root_port: 1, route: 0, depth: 0, speed: Speed::Full, parent: None, tt: None };
    let dev = Arc::new(Device {
        id: 1, host: host.clone(), slot: HcSlot(1), address: 1, place, desc, config: None,
        gone: AtomicBool::new(false), bind_lock: Mutex::named("t.bind", ()), alts: Default::default(),
    });
    let got = Arc::new(StdMutex::new(Vec::new()));
    let g = got.clone();
    let _h = dev.interrupt_pipe_at(0x81, PipeOpts::new(8, 2), Arc::new(move |e: PipeEvent<'_>, at: u64| {
        if let PipeEvent::Data(r) = e {
            g.lock().unwrap().push((r[0], at));
        }
    })).unwrap();
    let sink = host.sink.lock().unwrap().clone().expect("the untimed pipe");
    host.now.store(5000, Ordering::Release);
    sink(PipeEvent::Data(&[4]));
    assert_eq!(*got.lock().unwrap(), [(4, 5000)]);
}
