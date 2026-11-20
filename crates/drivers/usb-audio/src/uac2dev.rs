//! UAC2 functions (GP-SOUND AU-3): the Gaming-PC's board jacks, ASRock 26ce:0a09 (speaker, headphones and S/PDIF on
//! adaptive high-speed OUT endpoints). Only an AudioControl interface with bInterfaceProtocol 0x20 comes here
//! (`DRIVER_UAC2` in lib.rs), and it is bound on every machine; the UAC1 probe in `dev.rs` is not on
//! this path and sends what it sent before.
//!
//! Probe (hotplug worker, may sleep), per output jack of the device (Linux 7.3.1 `sound/usb`):
//! 1. the configuration descriptor's bytes once more (usb-core keeps only its parsed form), parsed whole by
//!    `uac2::desc::Function::parse`, which refuses a broken tree as a whole;
//! 2. the jack's clock and its RANGE (format.c:639-693), the plan S16 stereo 48 kHz (`uac2::plan`);
//! 3. the UAC2 order (endpoint.c:1498-1530): alternate setting 0, read the rate, set it if different and read it
//!    back (clock.c:561-598, 600-660), clock valid (clock.c:250-292, 665-667), the alternate setting last;
//! 4. its feature unit: mute as the master switch says, volume 0 dB inside its RANGE (mixer.c:338-391, 460-498).
//!
//! A jack that fails is left out with one line naming the step and what the device answered; the others play. The
//! settings stay while the device is there, as UAC1's do, so the mixer's thread opens a stream without a control
//! transfer. Refused by name, as `uac2` refuses them: selector or multiplier clocks, mixer or selector units on the
//! path, asynchronous OUT that needs feedback. The board's microphones are not offered yet (no capture here).

use crate::dev;
use crate::plan::{StreamPlan, Sync};
use crate::req::Ctl;
use crate::uac;
use crate::words;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use ksync::SpinLock;
use uac2::clock::{self as clk, Step};
use uac2::desc::{ClockSource, FeatureUnit, Function, terminal};
use uac2::feature::{self, MUTE, VOLUME};
use uac2::plan::{self as plan2, Plan, PlanError, Want};
use uac2::range::{self, RangeError, Ranges};
use usb_core::{Configuration, Device, DeviceId, Interface, Setup, Speed, UsbError};

klog::counter!(JACKS_UP, "usb_audio.uac2_jacks");

pub const RATE: u32 = 48_000;

/// What a jack's stream needs again later (the self-check redoes its start).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Jack {
    pub jack: u16,
    pub plan: Plan,
    pub clock: ClockSource,
    pub feature: Option<FeatureUnit>,
}

static JACKS: SpinLock<Vec<(DeviceId, u8, Vec<Jack>)>> = SpinLock::named("usb_audio.uac2_jacks", Vec::new());
/// What did not come up, in plain German, for the report page: (device, control interface, line).
static REFUSED: SpinLock<Vec<(DeviceId, u8, String)>> = SpinLock::named("usb_audio.uac2_refused", Vec::new());

/// The lines of jacks and UAC2 devices that did not come up, while their device is there (a device whose probe
/// failed is never disconnected from this driver, so its lines go when it leaves the registry). Each probe replaces
/// its function's lines: usb-core probes an unbound function again whenever a driver registers.
pub fn refused() -> Vec<String> {
    let live: Vec<DeviceId> = usb_core::registry::devices().iter().filter(|d| !d.is_gone()).map(|d| d.id).collect();
    let mut l = REFUSED.lock();
    l.retain(|r| live.contains(&r.0));
    l.iter().map(|r| r.2.clone()).collect()
}

/// The latest probe's verdict on function (`id`, `control`) replaces what an earlier probe said.
pub fn note(list: &mut Vec<(DeviceId, u8, String)>, id: DeviceId, control: u8, lines: Vec<String>) {
    list.retain(|r| !(r.0 == id && r.1 == control));
    list.extend(lines.into_iter().map(|l| (id, control, l)));
}

/// The UAC2 jack behind playback interface `iface` of device `id`.
pub fn jack(id: DeviceId, iface: u8) -> Option<Jack> {
    JACKS.lock().iter().filter(|j| j.0 == id).flat_map(|j| j.2.iter()).find(|j| j.plan.iface == iface).cloned()
}

pub(crate) fn conv(s: clk::Setup) -> Setup {
    Setup { request_type: s.request_type, request: s.request, value: s.value, index: s.index, length: s.length }
}

/// The jacks a stream can play to: every output terminal that is not the USB side, in descriptor order.
pub fn output_jacks(f: &Function) -> Vec<u16> {
    f.outputs.iter().filter(|t| t.ty != terminal::USB_STREAMING).map(|t| t.ty).collect()
}

/// What the start order did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Started {
    /// The clock's rate before (None: the device did not say).
    pub before: Option<u32>,
    pub wrote: bool,
    /// Read back after setting, or the unchanged rate.
    pub after: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartError {
    Deselect(UsbError),
    SetRate { err: UsbError, before: Option<u32> },
    /// The clock says it is not valid at the rate (clock.c:665-667: Linux gives up there, -ENXIO).
    Invalid { rate: Option<u32> },
    Select(UsbError),
}

fn read_rate(c: &dyn Ctl, s: clk::Setup) -> Option<u32> {
    let mut b = [0u8; 4];
    match c.input(conv(s), &mut b) {
        Ok(4) => clk::decode_rate(&b),
        _ => None,
    }
}

/// The UAC2 start of one stream in `uac2::clock::start_order`'s order.
pub fn start(c: &dyn Ctl, clock: &ClockSource, p: &Plan) -> Result<Started, StartError> {
    let mut st = Started::default();
    for step in clk::start_order(p.control_iface, p.clock, p.rate, p.iface, p.alt) {
        match step {
            Step::Interface { iface, alt: 0 } => c.set_interface(iface, 0).map_err(StartError::Deselect)?,
            Step::ReadRate(s) => st.before = read_rate(c, s),
            Step::WriteRateIfDifferent(s, data) => {
                // clock.c:529-547 and 629-633: a rate the device does not answer counts as 0, so it is set.
                if clk::needs_set(st.before.unwrap_or(0), p.rate) {
                    c.out(conv(s), &data).map_err(|err| StartError::SetRate { err, before: st.before })?;
                    st.wrote = true;
                    st.after = read_rate(c, clk::get_rate(p.control_iface, p.clock));
                } else {
                    st.after = st.before;
                }
            }
            Step::ReadValid(s) => {
                let mut b = [0u8; 1];
                let reply = if clock.validity_readable() { c.input(conv(s), &mut b).ok().filter(|&n| n == 1) } else { None };
                if !clk::decode_valid(clock, reply.map(|_| &b[..])) {
                    return Err(StartError::Invalid { rate: st.after });
                }
            }
            Step::Interface { iface, alt } => c.set_interface(iface, alt).map_err(StartError::Select)?,
        }
    }
    Ok(st)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FeatureDone {
    pub mutes: u32,
    pub volumes: u32,
    pub failed: u32,
    pub level: i16,
}

/// Mute on every channel that has it writable, volume 0 dB inside the unit's RANGE on every channel with volume.
pub fn apply_features(c: &dyn Ctl, control: u8, fu: &FeatureUnit, mute: bool) -> FeatureDone {
    let mut r = FeatureDone::default();
    for ch in feature::writable(fu, MUTE) {
        let (s, d) = feature::set_mute(control, fu.id, ch, mute);
        match c.out(conv(s), &d) {
            Ok(_) => r.mutes += 1,
            Err(_) => r.failed += 1,
        }
    }
    for ch in feature::writable(fu, VOLUME) {
        let mut b = [0u8; feature::VOLUME_RANGE_BYTES as usize];
        let range = c.input(conv(feature::volume_range(control, fu.id, ch)), &mut b).ok().and_then(|n| feature::decode_volume_range(&b[..n]));
        let level = feature::unity(range);
        let (s, d) = feature::set_volume(control, fu.id, ch, level);
        match c.out(conv(s), &d) {
            Ok(_) => {
                r.volumes += 1;
                r.level = level;
            }
            Err(_) => r.failed += 1,
        }
    }
    r
}

/// The unit as UAC1's `req::set_mute` reads it: the mute bit on each channel whose mute is writable, nothing else.
/// Its request (0x21, bRequest 1, MUTE << 8 | channel, unit << 8 | interface, one byte) is UAC2's CUR byte for byte
/// (mixer.c:466-498), so the master switch reaches the board's jacks unchanged; no volume bit, so UAC1's GET_MIN and
/// GET_MAX never go to a UAC2 device.
fn mute_only(fu: &FeatureUnit) -> uac::FeatureUnit {
    let mut controls = vec![0u32; fu.controls.len()];
    for ch in feature::writable(fu, MUTE) {
        controls[ch as usize] = uac::FU_MUTE;
    }
    uac::FeatureUnit { id: fu.id, source: fu.source, controls }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JackError {
    Plan(PlanError),
    Range(UsbError),
    RangeBytes(RangeError),
    /// The plan's endpoint is not in usb-core's parse of the same configuration.
    NoEndpoint(u8),
    /// The descriptor read again for the plan says something else of this endpoint than usb-core's first read, on
    /// whose endpoint the stream would run.
    DescriptorChanged(u8),
    Start { error: StartError, clock: u8, iface: u8, alt: u8 },
}

/// RANGE of a clock's frequency: two bytes for the count, then exactly the table (format.c:639-693).
pub fn read_range(c: &dyn Ctl, control: u8, clock: u8) -> Result<Vec<u8>, JackError> {
    let mut head = [0u8; 2];
    let n = c.input(conv(clk::range_head(control, clock)), &mut head).map_err(JackError::Range)?;
    let len = range::full_length(&head[..n]).map_err(JackError::RangeBytes)?;
    let mut full = vec![0u8; len as usize];
    let n = c.input(conv(clk::range_full(control, clock, len)), &mut full).map_err(JackError::Range)?;
    full.truncate(n);
    Ranges::parse(&full).map_err(JackError::RangeBytes)?;
    Ok(full)
}

/// The mixer's side of a jack: UAC1's stream shape, no UAC1 rate control (the clock was set here).
pub fn stream_plan(cfg: &Configuration, j: &Jack) -> Result<StreamPlan, JackError> {
    let p = &j.plan;
    let a = p.endpoint.address;
    let ep = cfg.interface(p.iface, p.alt).and_then(|i| i.endpoints.iter().find(|e| e.address == a).copied()).ok_or(JackError::NoEndpoint(a))?;
    if (ep.attributes, ep.max_packet_raw, ep.interval) != (p.endpoint.attributes, p.endpoint.max_packet, p.endpoint.interval) {
        return Err(JackError::DescriptorChanged(a));
    }
    Ok(StreamPlan {
        control: p.control_iface,
        iface: p.iface,
        alt: p.alt,
        ep,
        playback: p.playback,
        channels: p.channels,
        subframe: p.subslot,
        bits: p.bits,
        sync: Sync::of(&ep),
        rate_control: false,
        rate_only: false,
        terminal_type: j.jack,
        features: j.feature.iter().map(mute_only).collect(),
    })
}

/// One jack from its clock to its feature unit. The plan and the endpoint are checked before anything is sent that
/// changes the device. `full_speed`: the device's speed, which sets what its endpoint interval counts.
pub fn bring_up_jack(c: &dyn Ctl, cfg: &Configuration, f: &Function, jack: u16, mute: bool, full_speed: bool)
    -> Result<(Jack, StreamPlan, Started, FeatureDone), JackError>
{
    let want = Want { jack, rate: RATE, channels: 2, bits: 16, full_speed };
    let clock_id = plan2::clock_for(f, jack).map_err(JackError::Plan)?;
    let clock = *f.clock(clock_id).ok_or(JackError::Plan(PlanError::ClockNotASource(clock_id)))?;
    if !clock.frequency_writable() {
        return Err(JackError::Plan(PlanError::ClockFixed(clock_id)));
    }
    let rates = read_range(c, f.control_iface, clock_id)?;
    let ranges = Ranges::parse(&rates).map_err(JackError::RangeBytes)?;
    let plan = plan2::plan(f, &want, &ranges).map_err(JackError::Plan)?;
    let j = Jack { jack, plan, clock, feature: plan.feature.and_then(|id| f.feature(id)).cloned() };
    let sp = stream_plan(cfg, &j)?;
    let st = start(c, &clock, &plan).map_err(|error| JackError::Start { error, clock: plan.clock, iface: plan.iface, alt: plan.alt })?;
    let fd = j.feature.as_ref().map(|fu| apply_features(c, f.control_iface, fu, mute)).unwrap_or_default();
    Ok((j, sp, st, fd))
}

fn read_config(c: &dyn Ctl, total: u16) -> Result<Vec<u8>, UsbError> {
    let mut b = vec![0u8; total as usize];
    let n = c.input(Setup::get_descriptor(2, 0, total), &mut b)?;
    b.truncate(n);
    Ok(b)
}

pub fn probe(dev: &Arc<Device>, f: &Interface) -> Result<(), String> {
    if let Some(k) = dev::kept_off() {
        return Err(format!("left unbound while {k}"));
    }
    let mut lines = Vec::new();
    let r = probe_lines(dev, f, &mut lines);
    note(&mut REFUSED.lock(), dev.id, f.number, lines);
    r
}

fn probe_lines(dev: &Arc<Device>, f: &Interface, lines: &mut Vec<String>) -> Result<(), String> {
    let cfg = dev.config.as_ref().ok_or("no configuration")?;
    let c: &dyn Ctl = dev;
    let bytes = read_config(c, cfg.total_length).map_err(|e| {
        lines.push(format!("USB-Tongerät: Beschreibung nicht gelesen: {}", words::usb(e)));
        format!("configuration descriptor not read again: {}", e.as_str())
    })?;
    let func = Function::parse(&bytes).map_err(|e| {
        lines.push(format!("USB-Tongerät: {}", words::descriptors(&e)));
        format!("UAC2 descriptors refused: {e:?}")
    })?;
    if func.control_iface != f.number {
        return Err(format!("UAC2 control interface is {}, offered {}", func.control_iface, f.number));
    }
    let mute = dev::hw_mute();
    let full_speed = dev.speed() == Speed::Full;
    let names = output_jacks(&func);
    let (mut jacks, mut playback) = (Vec::new(), Vec::new());
    for &jack in &names {
        match bring_up_jack(c, cfg, &func, jack, mute, full_speed) {
            Ok((j, sp, st, fd)) => {
                if st.after.is_some_and(|r| r != RATE) {
                    // clock.c:646-650: Linux goes on too; the stream would play at the clock's pace.
                    klog::event!(Warn, "usb_audio", "uac2.rate_mismatch", device = dev.path(), jack = klog::Hex(jack as u64), read = st.after.unwrap_or(0));
                }
                klog::event!(Info, "usb_audio", "uac2.jack", device = dev.path(), jack = klog::Hex(jack as u64), interface = j.plan.iface,
                    alt = j.plan.alt, clock = j.plan.clock, before = st.before.unwrap_or(0), wrote = st.wrote, after = st.after.unwrap_or(0),
                    mutes = fd.mutes, volumes = fd.volumes, failed = fd.failed, level_db256 = fd.level as i64);
                JACKS_UP.inc();
                jacks.push(j);
                playback.push(sp);
            }
            Err(e) => {
                klog::event!(Warn, "usb_audio", "uac2.jack_refused", device = dev.path(), jack = klog::Hex(jack as u64), why = format!("{e:?}").as_str();
                    "this jack is left out; the others play");
                lines.push(format!("{}: {}", words::jack_name(jack), words::jack(&e)));
            }
        }
    }
    if playback.is_empty() {
        return Err(format!("no UAC2 jack could be started ({} tried)", names.len()));
    }
    JACKS.lock().push((dev.id, f.number, jacks));
    dev::add(dev, f.number, playback, Vec::new(), mute, names.len() > 1);
    Ok(())
}

pub fn disconnect(dev: &Arc<Device>, iface: u8) {
    JACKS.lock().retain(|j| !(j.0 == dev.id && j.1 == iface));
    REFUSED.lock().retain(|j| !(j.0 == dev.id && j.1 == iface));
    dev::disconnect(dev, iface);
}
