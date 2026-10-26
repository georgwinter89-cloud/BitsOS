//! What a host controller driver (xHCI today) does for the core: the slot of a device, endpoint 0, endpoints of
//! a configuration, hub slots, interrupt IN pipes, and waking its hotplug worker. The core never sees TRBs or
//! contexts; the controller never sees descriptors beyond the endpoints it must set up.

use crate::desc::Endpoint;
use crate::place::Place;
use crate::setup::Setup;
use crate::iso::{IsoRequest, IsoUrb};
use crate::transfer::{Buffer, PipeOpts, Request, Urb};
use alloc::sync::Arc;

/// A device's handle in its host controller (opaque to the core; xHCI packs slot ID and controller epoch).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HcSlot(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsbError {
    /// The controller is not running (quiesced, failed, restarting).
    NotRunning,
    /// The device was detached; nothing reaches it any more.
    Gone,
    Timeout,
    /// The transfer was cancelled (`Transfer::cancel`, `Device::abort`, a pipe stopped, endpoints replaced).
    Cancelled,
    Stall,
    Babble,
    /// USB transaction error (no handshake, CRC, timeout on the bus).
    Transaction,
    /// Another completion code of a transfer.
    Transfer(u8),
    /// A command failed with this completion code.
    Command(u8),
    /// The controller has no free device slot.
    NoSlot,
    NoMemory,
    /// The slot is busy with another operation for longer than allowed.
    Busy,
    /// Too many hub tiers (route string full).
    TooDeep,
    /// A descriptor was short or of the wrong type.
    Descriptor,
    Invalid(&'static str),
}

/// The data stage of a control transfer.
pub enum Data<'a> {
    None,
    In(&'a mut [u8]),
    Out(&'a [u8]),
}

/// What an interrupt IN pipe delivers. Called in the host's event context: the sink must not sleep or wait.
pub enum PipeEvent<'a> {
    Data(&'a [u8]),
    /// One failed transfer; the host repairs the endpoint and keeps polling.
    Error(UsbError),
    /// The pipe ended for good (device gone, too many errors in a row): no event follows.
    Stopped(UsbError),
}

pub type Sink = Arc<dyn Fn(PipeEvent<'_>) + Send + Sync>;

/// A pipe sink that also gets, with each event, the time (ns, the host's `now_ns` clock) the controller's
/// interrupt for it arrived: the input stamp of B4 (IN-6), taken before the event waits for any thread.
pub type TimedSink = Arc<dyn Fn(PipeEvent<'_>, u64) + Send + Sync>;

/// What a hub's slot must tell the controller (xHCI slot context Hub, Number of Ports, TT Think Time, MTT).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HubParams {
    pub ports: u8,
    /// wHubCharacteristics bits 6:5 of a high speed hub (0 otherwise).
    pub tt_think_time: u8,
    pub multi_tt: bool,
}

pub trait Host: Send + Sync {
    fn name(&self) -> &str;
    fn now_ns(&self) -> u64;
    /// Sleeps the calling thread (power-good waits, recovery times).
    fn sleep_ns(&self, ns: u64);
    /// A slot for the device just reset at `place`, addressed (SET_ADDRESS sent) with endpoint 0 at `mps0`.
    /// Returns the handle and the USB address the controller gave it. `deferred`: SET_ADDRESS is not sent yet
    /// (xHCI BSR 1); the device answers at address 0 (address returned: 0) until `set_address`.
    fn address(&self, place: &Place, mps0: u16, deferred: bool) -> Result<(HcSlot, u8), UsbError>;
    /// Sends the SET_ADDRESS held back by a deferred `address`; returns the address.
    fn set_address(&self, slot: HcSlot) -> Result<u8, UsbError>;
    /// Endpoint 0's real packet size (Evaluate Context with the whole endpoint 0 context).
    fn set_mps0(&self, slot: HcSlot, mps0: u16) -> Result<(), UsbError>;
    /// A control transfer on endpoint 0; returns the bytes of the data stage actually transferred.
    fn control(&self, slot: HcSlot, setup: Setup, data: Data<'_>) -> Result<usize, UsbError>;
    /// The endpoints of the configuration about to be set (Configure Endpoint); replaces earlier ones.
    fn configure(&self, slot: HcSlot, endpoints: &[Endpoint]) -> Result<(), UsbError>;
    /// Makes the slot a hub's.
    fn set_hub(&self, slot: HcSlot, hub: HubParams) -> Result<(), UsbError>;
    /// Polls interrupt IN endpoint `ep` (address) with `opts.depth` transfers of `opts.len` bytes queued until the
    /// device goes or `stop_pipe`; each result goes to `sink`. Errors are repaired by the host's worker.
    fn interrupt_in(&self, slot: HcSlot, ep: u8, opts: PipeOpts, sink: Sink) -> Result<(), UsbError>;
    /// Like `interrupt_in`, each event with the time its controller interrupt arrived. `None`: this host does not
    /// track it (the default); `Device::interrupt_pipe_at` then stamps when the event reaches the sink.
    fn interrupt_in_at(&self, _slot: HcSlot, _ep: u8, _opts: PipeOpts, _sink: &TimedSink) -> Option<Result<(), UsbError>> {
        None
    }
    /// Stops the pipe on `ep`: its transfers cancelled, the sink gets `Stopped(Cancelled)`.
    fn stop_pipe(&self, slot: HcSlot, ep: u8) -> Result<(), UsbError>;
    /// DMA memory for transfers of this host (its address limit), zeroed.
    fn alloc_buffer(&self, len: usize) -> Result<Buffer, UsbError>;
    /// Queues a bulk or interrupt transfer on `ep` (address) behind those already queued. Never sleeps (callable
    /// from a completion callback). `urb` completes when it ends.
    fn submit(&self, slot: HcSlot, ep: u8, req: Request, urb: Arc<Urb>) -> Result<(), UsbError>;
    /// Sleeps until `urb` completed, at most `timeout_ns`; true if it did.
    fn wait(&self, slot: HcSlot, urb: &Urb, timeout_ns: u64) -> bool;
    /// Takes `urb` off its endpoint (stopping the endpoint if it runs) and completes it with `why` unless it
    /// completed first. Afterwards the URB is always complete.
    fn cancel(&self, slot: HcSlot, ep: u8, urb: &Arc<Urb>, why: UsbError);
    /// Brings `ep` back with data toggle 0 on the host side (Reset Endpoint after a halt; otherwise Stop Endpoint
    /// and Configure Endpoint dropping and adding it); transfers queued behind a failed one run again.
    fn reset_endpoint(&self, slot: HcSlot, ep: u8) -> Result<(), UsbError>;
    /// Cancels every transfer queued on `ep` (they end with `Cancelled`); returns how many.
    fn abort_endpoint(&self, slot: HcSlot, ep: u8) -> Result<usize, UsbError>;
    /// SET_INTERFACE's host side, before the request goes to the device: the endpoints of the interface's old
    /// alternate setting (`drop`) are removed and those of the new one (`add`) set up; whatever was queued on a
    /// dropped endpoint ends. A controller that cannot give the new endpoints their bandwidth refuses
    /// (xHCI: `Command(8)`, Bandwidth Error) and keeps the old ones. The default: not supported.
    fn set_interface(&self, _slot: HcSlot, _drop: &[Endpoint], _add: &[Endpoint]) -> Result<(), UsbError> {
        Err(UsbError::Invalid("this host cannot change alternate settings"))
    }
    /// Queues an isochronous request on `ep` (address) behind those already queued, one packet per service
    /// interval (`iso` module). Never sleeps. `urb` completes once, with every packet's result. The default: not
    /// supported.
    fn submit_iso(&self, _slot: HcSlot, _ep: u8, _req: IsoRequest, _urb: Arc<IsoUrb>) -> Result<(), UsbError> {
        Err(UsbError::Invalid("this host has no isochronous transfers"))
    }
    /// The device is gone or given up: Disable Slot, its contexts and rings freed, pipes stopped.
    fn release(&self, slot: HcSlot);
    /// Wakes the hotplug worker that serves this host (a hub reported a change).
    fn kick(&self);
}

impl UsbError {
    pub fn as_str(&self) -> &'static str {
        match self {
            UsbError::NotRunning => "controller not running",
            UsbError::Gone => "device gone",
            UsbError::Timeout => "timeout",
            UsbError::Cancelled => "cancelled",
            UsbError::Stall => "stall",
            UsbError::Babble => "babble",
            UsbError::Transaction => "transaction error",
            UsbError::Transfer(_) => "transfer error",
            UsbError::Command(_) => "command failed",
            UsbError::NoSlot => "no free slot",
            UsbError::NoMemory => "no DMA memory",
            UsbError::Busy => "busy",
            UsbError::TooDeep => "nested too deep",
            UsbError::Descriptor => "bad descriptor",
            UsbError::Invalid(why) => why,
        }
    }
}
