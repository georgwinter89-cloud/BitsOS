//! GX-7c: a fake PMFW (SMU 13.0.4, MP1) for the bring-up tests, shaped after the 27.10. Linux capture of the
//! real 780M. Two mailboxes on the one firmware (smu_v13_0_4_ppt.c:1127-1134, dcn314_smu.c): the driver's
//! (message C2PMSG_66, argument 82, response 90) and the display's (67/83/91), each with its own message in
//! flight. The protocol of smu_cmn.c:301-393: the driver writes the response 0, the argument, the message; PMFW
//! answers in the response (1 OK, 0xFF failed, 0xFE unknown, 0xFD prerequisites, 0xFC busy) and leaves its
//! answer in the argument. FIRMWARE_FLAGS is read through the SMN window (`smn_read`).
//!
//! Answers and latencies are the capture's: GetDriverIfVersion 8, GetPmfwVersion 0x004c5700, the feature masks,
//! EnableGfxImu answered after 29.3 ms (Linux sends it without waiting, smu_v13_0.c:2302; the next message's
//! pre-poll waited 5418 reads; BitsOS on the Mini-PC 28.10.: 30244 us, the fake's value), PowerUpVcn 1.28 ms.
//! GfxDeviceDriverReset(2) is never answered unless `mode2_answers` (the Mini-PC 28.10.: no answer after a GC
//! stage without Linux's suspend around it, then the box hung; gx-GX-7f.md). Messages that change GC's power go out as `SmuEvent`s for
//! the GC fake (`gcfake`). `SmuHostile` entries compose: each changes one message or the whole firmware.

use super::psp::Hw;
use alloc::vec::Vec;

pub use crate::ipmod::smu13::{msg, resp};

/// What the real 780M answered on 27.10. (the capture).
pub const PMFW_VERSION: u32 = 0x004c_5700;
pub const DRIVER_IF_VERSION: u32 = 8;
pub const FEATURES_LO: u32 = 0xf3ff_f7ff;
pub const FEATURES_HI: u32 = 0x07a5_f3f3;
/// FIRMWARE_FLAGS through the SMN window; bit 0 INTERRUPTS_ENABLED (the capture read 0x1).
pub use crate::ipmod::smu13::{FIRMWARE_FLAGS_UP, SMN_FIRMWARE_FLAGS};

/// The messages Linux 7.1 sent on the driver's mailbox during the 27.10. load, in order: (id, argument, the
/// answer it read back in the argument register, or `None` when it read none). Every answer was OK (1).
pub const LINUX_LOAD: &[(u32, u32, Option<u32>)] = &[
    (msg::GET_DRIVER_IF_VERSION, 0, Some(DRIVER_IF_VERSION)),
    (msg::GET_PMFW_VERSION, 0, Some(PMFW_VERSION)),
    (msg::ENABLE_GFX_IMU, 1, None),
    (msg::POWER_UP_VCN, 0, None),
    (msg::POWER_UP_JPEG, 0, None),
    (msg::SET_DRIVER_DRAM_ADDR_HIGH, 0x80, None),
    (msg::SET_DRIVER_DRAM_ADDR_LOW, 0x1e_d000, None),
    (msg::GET_ENABLED_SMU_FEATURES, 0, Some(FEATURES_LO)),
    (msg::GET_ENABLED_SMU_FEATURES, 1, Some(FEATURES_HI)),
    (msg::GET_ENABLED_SMU_FEATURES, 0, Some(FEATURES_LO)),
    (msg::GET_ENABLED_SMU_FEATURES, 1, Some(FEATURES_HI)),
    (msg::TRANSFER_TABLE_SMU2DRAM, 4, None),
    (msg::GET_ENABLED_SMU_FEATURES, 0, Some(FEATURES_LO)),
    (msg::GET_ENABLED_SMU_FEATURES, 1, Some(FEATURES_HI)),
    (msg::GET_ENABLED_SMU_FEATURES, 0, Some(FEATURES_LO)),
    (msg::GET_ENABLED_SMU_FEATURES, 1, Some(FEATURES_HI)),
    (msg::GET_ENABLED_SMU_FEATURES, 0, Some(FEATURES_LO)),
    (msg::GET_ENABLED_SMU_FEATURES, 1, Some(FEATURES_HI)),
    (msg::TRANSFER_TABLE_SMU2DRAM, 7, None),
    (msg::POWER_DOWN_JPEG, 0, None),
    (msg::POWER_DOWN_VCN, 0, None),
];

/// The answer latency in us: the capture's (first message's write to the first non-zero response), 10 us for
/// the rest.
pub fn latency_us(id: u32) -> u64 {
    match id {
        msg::ENABLE_GFX_IMU => 30_244,
        msg::POWER_UP_VCN => 1_280,
        msg::SET_DRIVER_DRAM_ADDR_LOW => 750,
        msg::POWER_UP_JPEG => 590,
        msg::GET_DRIVER_IF_VERSION => 120,
        _ => 10,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Client {
    Driver,
    Display,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmuHostile {
    /// PMFW does not run: no message is ever answered, FIRMWARE_FLAGS reads 0.
    Dead,
    /// Messages with this id answer `code` (0xFF, 0xFE, 0xFD, 0xFC, ...).
    Answer { id: u32, code: u32 },
    /// Messages with this id answer only after `us`.
    Late { id: u32, us: u64 },
    /// Messages with this id are never answered (PMFW stuck on it).
    Never { id: u32 },
    /// GX-7e: messages with this id answer OK with `value` in the argument register (another PMFW).
    Value { id: u32, value: u32 },
}

/// What a message did to GC's power, for the GC fake (applied when PMFW takes the message).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmuEvent {
    ImuStart,
    AllowGfxOff,
    DisallowGfxOff,
    /// GfxDeviceDriverReset (mode 2): GC is reset (BitsOS never sends it during bring-up).
    GcReset,
    /// GX-7j3: PrepareMp1ForUnload: PMFW lets go of the GFX driver's GC (the fake changes no GC power for it).
    Unload,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mailbox {
    /// BAR5 bytes of message, argument, response.
    pub msg_at: u32,
    pub arg_at: u32,
    pub resp_at: u32,
    pub msg: u32,
    pub arg: u32,
    pub resp: u32,
    /// (due, id, argument) of the message PMFW is working on; due `u64::MAX`: never.
    pub flight: Option<(u64, u32, u32)>,
}

pub struct SmuFake {
    pub drv: Mailbox,
    pub dcn: Mailbox,
    pub hostile: Vec<SmuHostile>,
    /// Every message PMFW took: (client, id, argument, time).
    pub taken: Vec<(Client, u32, u32, u64)>,
    /// A message written while that mailbox's previous one was still in flight (the pre-poll was skipped).
    pub overlaps: Vec<(Client, u32)>,
    pub events: Vec<SmuEvent>,
    pub dram: (u32, u32),
    pub vcn_on: bool,
    pub jpeg_on: bool,
    /// GfxDeviceDriverReset is answered (false: never, as on the Mini-PC 28.10.).
    pub mode2_answers: bool,
    /// GX-7j3: PrepareMp1ForUnload was taken since the last EnableGfxImu (Linux's unload, shutdown or kexec).
    pub unloaded: bool,
    /// GX-7j3, the Mini-PC 29.10. model: firmware loaded again (AUTOLOAD_RLC) under an IMU PMFW still runs for a
    /// GFX driver that never unloaded leaves PMFW answering no driver message again (`wedged`; the real one
    /// stayed so until a cold boot). On by default: the fakes behave as the chip did.
    pub wedge_on_warm_load: bool,
    pub wedged: bool,
    /// GX-9b: MinGfxClk and MaxGfxClk of PMFW's DpmClocks table; the limits in force; the load PMFW measures (%);
    /// the temperature and power its metrics report; what a transfer put in memory: (MC address, bytes).
    pub gfx_range: (u32, u32),
    pub hard_min: u32,
    pub soft_max: u32,
    pub load: u32,
    pub metrics_read: u32,
    pub table_writes: Vec<(u64, Vec<u8>)>,
}

/// GX-9b: the fake's GFXCLK range (an assumption; the Mini-PC's own answer is `gpu`'s level line).
pub const GFX_RANGE: (u32, u32) = (800, 2700);

impl SmuFake {
    /// The mailboxes at MP1's segment-1 base (dwords). Cold: both last answers OK (the capture's first read of
    /// C2PMSG_91 was 0x1; linux-6.6's first message pre-polls C2PMSG_90 and gives up on 0, smu_cmn.c:369, so the
    /// BIOS leaves it non-zero: an assumption, the capture's 7.1 kernel skipped that read), no message id.
    pub fn new(mp1_seg1: u32) -> SmuFake {
        let at = |dw: u32| mp1_seg1.saturating_add(dw).saturating_mul(4);
        SmuFake {
            drv: Mailbox { msg_at: at(0x282), arg_at: at(0x292), resp_at: at(0x29a), resp: resp::OK, ..Mailbox::default() },
            dcn: Mailbox { msg_at: at(0x283), arg_at: at(0x293), resp_at: at(0x29b), resp: resp::OK, ..Mailbox::default() },
            hostile: Vec::new(),
            taken: Vec::new(),
            overlaps: Vec::new(),
            events: Vec::new(),
            dram: (0, 0),
            vcn_on: false,
            jpeg_on: false,
            mode2_answers: false,
            unloaded: false,
            wedge_on_warm_load: true,
            wedged: false,
            gfx_range: GFX_RANGE,
            hard_min: GFX_RANGE.0,
            soft_max: GFX_RANGE.1,
            load: 0,
            metrics_read: 0,
            table_writes: Vec::new(),
        }
    }

    /// GX-9b: DpmClocks_t as PMFW writes it (the two fields Linux reads; the rest stays empty, display clocks too).
    fn clocks_table(&self) -> Vec<u8> {
        let mut t = alloc::vec![0u8; crate::dpm_table::DPMCLOCKS_BYTES];
        t[crate::dpm_table::MIN_GFX_AT..crate::dpm_table::MIN_GFX_AT + 4].copy_from_slice(&self.gfx_range.0.to_le_bytes());
        t[crate::dpm_table::MAX_GFX_AT..crate::dpm_table::MAX_GFX_AT + 4].copy_from_slice(&self.gfx_range.1.to_le_bytes());
        t
    }

    /// GX-9b: SmuMetrics_t: GFXCLK by load between the limits in force, the capture's pm_info for the rest.
    fn metrics_table(&mut self) -> Vec<u8> {
        self.metrics_read = self.metrics_read.wrapping_add(1);
        let gfx = if self.load > 0 { self.soft_max } else { self.hard_min };
        let mut t = alloc::vec![0u8; crate::dpm_table::METRICS_BYTES];
        let mut put = |at: usize, v: u16| t[at..at + 2].copy_from_slice(&v.to_le_bytes());
        put(0, gfx as u16);
        put(2, 1200);
        put(8, 2800);
        put(12, (self.load * 100) as u16);
        put(16, 1165);
        put(18, 762);
        put(80, 3900);
        put(82, 4150);
        put(86, 9000);
        put(120, gfx as u16);
        put(124, (self.load * 100) as u16);
        put(136, 8500);
        t[92..96].copy_from_slice(&7200u32.to_le_bytes());
        t[156..160].copy_from_slice(&self.metrics_read.to_le_bytes());
        t
    }

    /// GX-7j3: the PSP took AUTOLOAD_RLC; `imu_running`: PMFW's IMU was started by an earlier load.
    pub fn warm_load(&mut self, imu_running: bool) {
        if self.wedge_on_warm_load && imu_running && !self.unloaded && !self.wedged {
            self.wedged = true;
        }
    }

    fn dead(&self) -> bool {
        self.hostile.contains(&SmuHostile::Dead)
    }

    pub fn owns(&self, byte: u32) -> bool {
        [self.drv, self.dcn].iter().any(|m| byte == m.msg_at || byte == m.arg_at || byte == m.resp_at)
    }

    pub fn read(&mut self, byte: u32) -> Option<u32> {
        for m in [&self.drv, &self.dcn] {
            if byte == m.msg_at {
                return Some(m.msg);
            }
            if byte == m.arg_at {
                return Some(m.arg);
            }
            if byte == m.resp_at {
                return Some(m.resp);
            }
        }
        None
    }

    /// `false`: not an SMU register.
    pub fn write(&mut self, byte: u32, value: u32, now: u64) -> bool {
        for c in [Client::Driver, Client::Display] {
            let m = if c == Client::Driver { &mut self.drv } else { &mut self.dcn };
            if byte == m.arg_at {
                m.arg = value;
                return true;
            }
            if byte == m.resp_at {
                m.resp = value;
                return true;
            }
            if byte == m.msg_at {
                m.msg = value;
                let (arg, flying) = (m.arg, m.flight.is_some());
                if flying {
                    self.overlaps.push((c, value));
                }
                self.take(c, value, arg, now);
                return true;
            }
        }
        false
    }

    fn take(&mut self, c: Client, id: u32, arg: u32, now: u64) {
        let mut due = now.saturating_add(latency_us(id));
        for h in &self.hostile {
            match *h {
                SmuHostile::Dead => due = u64::MAX,
                SmuHostile::Never { id: i } if i == id => due = u64::MAX,
                SmuHostile::Late { id: i, us } if i == id => due = now.saturating_add(us),
                _ => {}
            }
        }
        if c == Client::Driver && id == msg::GFX_DEVICE_DRIVER_RESET && !self.mode2_answers {
            due = u64::MAX;
        }
        if c == Client::Driver && self.wedged {
            due = u64::MAX;
        }
        let dead = self.dead();
        let m = if c == Client::Driver { &mut self.drv } else { &mut self.dcn };
        // A message written over one still in flight is not taken: it answers busy (the fake's choice: the real
        // PMFW's reaction is unknown, both drivers pre-poll so they never do it). A stuck one stays stuck.
        match m.flight {
            Some((d, _, _)) if d == u64::MAX => return,
            Some(_) => {
                m.flight = Some((now.saturating_add(latency_us(0)), u32::MAX, arg));
                return;
            }
            None => m.flight = Some((due, id, arg)),
        }
        if dead {
            return;
        }
        self.taken.push((c, id, arg, now));
        if c == Client::Driver {
            match id {
                msg::ENABLE_GFX_IMU => {
                    self.unloaded = false;
                    self.events.push(SmuEvent::ImuStart)
                }
                msg::PREPARE_MP1_FOR_UNLOAD => {
                    self.unloaded = true;
                    self.events.push(SmuEvent::Unload)
                }
                msg::ALLOW_GFXOFF => self.events.push(SmuEvent::AllowGfxOff),
                msg::DISALLOW_GFXOFF => self.events.push(SmuEvent::DisallowGfxOff),
                // Unanswered (the 28.10. model) it does nothing the fake could show.
                msg::GFX_DEVICE_DRIVER_RESET if self.mode2_answers => self.events.push(SmuEvent::GcReset),
                _ => {}
            }
        }
    }

    /// Answers the messages that are due.
    pub fn tick(&mut self, now: u64) {
        for c in [Client::Driver, Client::Display] {
            let m = if c == Client::Driver { self.drv } else { self.dcn };
            let Some((due, id, arg)) = m.flight else { continue };
            if due > now {
                continue;
            }
            let (code, out) = if id == u32::MAX { (resp::BUSY, arg) } else { self.answer(c, id, arg) };
            let m = if c == Client::Driver { &mut self.drv } else { &mut self.dcn };
            m.flight = None;
            m.resp = code;
            m.arg = out;
        }
    }

    fn answer(&mut self, c: Client, id: u32, arg: u32) -> (u32, u32) {
        if let Some(code) = self.hostile.iter().find_map(|h| match *h {
            SmuHostile::Answer { id: i, code } if i == id => Some(code),
            _ => None,
        }) {
            return (code, arg);
        }
        if let Some(value) = self.hostile.iter().find_map(|h| match *h {
            SmuHostile::Value { id: i, value } if i == id && c == Client::Driver => Some(value),
            _ => None,
        }) {
            return (resp::OK, value);
        }
        if c == Client::Display {
            // dcn314_smu.c's messages: GetSmuVersion answers the version, the rest echo their argument (the
            // capture's clock requests answered other values; the fake does not model clocks).
            return (resp::OK, if id == msg::GET_PMFW_VERSION { PMFW_VERSION } else { arg });
        }
        let out = match id {
            msg::TEST => arg.wrapping_add(1),
            msg::GET_PMFW_VERSION => PMFW_VERSION,
            msg::GET_DRIVER_IF_VERSION => DRIVER_IF_VERSION,
            msg::GET_ENABLED_SMU_FEATURES => match arg {
                0 => FEATURES_LO,
                1 => FEATURES_HI,
                _ => return (resp::FAILED, arg),
            },
            msg::SET_DRIVER_DRAM_ADDR_HIGH => {
                self.dram.0 = arg;
                arg
            }
            msg::SET_DRIVER_DRAM_ADDR_LOW => {
                self.dram.1 = arg;
                arg
            }
            msg::TRANSFER_TABLE_SMU2DRAM | msg::TRANSFER_TABLE_DRAM2SMU if self.dram == (0, 0) => return (resp::PREREQ, arg),
            msg::TRANSFER_TABLE_SMU2DRAM => {
                let table = match arg & 0xffff {
                    crate::dpm_table::TABLE_DPMCLOCKS => self.clocks_table(),
                    crate::dpm_table::TABLE_SMU_METRICS => self.metrics_table(),
                    _ => return (resp::FAILED, arg),
                };
                self.table_writes.push((u64::from(self.dram.0) << 32 | u64::from(self.dram.1), table));
                arg
            }
            msg::SET_HARD_MIN_GFXCLK => {
                self.hard_min = arg;
                arg
            }
            msg::SET_SOFT_MAX_GFXCLK => {
                self.soft_max = arg;
                arg
            }
            msg::POWER_UP_VCN => {
                self.vcn_on = true;
                arg
            }
            msg::POWER_DOWN_VCN => {
                self.vcn_on = false;
                arg
            }
            msg::POWER_UP_JPEG => {
                self.jpeg_on = true;
                arg
            }
            msg::POWER_DOWN_JPEG => {
                self.jpeg_on = false;
                arg
            }
            i if i == 0 || i >= msg::COUNT => return (resp::UNKNOWN, arg),
            _ => arg,
        };
        (resp::OK, out)
    }

    /// An SMN register through the PCIe index/data window (`None`: not one the fake has).
    pub fn smn_read(&self, addr: u32) -> Option<u32> {
        (addr == SMN_FIRMWARE_FLAGS).then(|| if self.dead() { 0 } else { FIRMWARE_FLAGS_UP })
    }

    /// GX-7j3: the driver's mailbox with `id` in flight, answered (OK) at `due` us.
    pub fn in_flight(&mut self, id: u32, arg: u32, due: u64) {
        self.drv.msg = id;
        self.drv.arg = arg;
        self.drv.resp = resp::NONE;
        self.drv.flight = Some((due, id, arg));
    }

    /// The driver's mailbox left as a crash leaves it: `id` sent and never answered.
    pub fn stale_message(&mut self, id: u32, arg: u32) {
        self.drv.msg = id;
        self.drv.arg = arg;
        self.drv.resp = resp::NONE;
        self.drv.flight = Some((u64::MAX, id, arg));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendError {
    /// The pre-poll found the previous message unanswered ("SMU: I'm not done with your previous command").
    PrePoll { last: u32 },
    /// No answer within the wait.
    Timeout,
    Refused,
}

/// Linux's way of sending one message (smu_cmn.c:350-391 with smu_cmn_wait_for_response :301), as a test driver
/// of the fake: pre-poll the response, response 0, argument, message, then (when `wait`) poll the response.
/// Returns (response, argument register). Polls every `poll_us`, gives up after `limit_us` (Linux: 2 s).
pub fn linux_send(hw: &mut dyn Hw, m: &Mailbox, id: u32, arg: u32, wait: bool, poll_us: u64, limit_us: u64) -> Result<(u32, u32), SendError> {
    let poll = |hw: &mut dyn Hw| -> Option<u32> {
        let start = hw.now_us();
        for _ in 0..=limit_us / poll_us.max(1) + 1 {
            let r = hw.reg_read(m.resp_at)?;
            if r != 0 {
                return Some(r);
            }
            if hw.now_us().saturating_sub(start) >= limit_us {
                break;
            }
            hw.delay_us(poll_us.max(1));
        }
        None
    };
    if poll(hw).is_none() {
        return Err(SendError::PrePoll { last: hw.reg_read(m.msg_at).unwrap_or(u32::MAX) });
    }
    for (at, v) in [(m.resp_at, 0), (m.arg_at, arg), (m.msg_at, id)] {
        if !hw.reg_write(at, v) {
            return Err(SendError::Refused);
        }
    }
    if !wait {
        return Ok((resp::NONE, arg));
    }
    let r = poll(hw).ok_or(SendError::Timeout)?;
    Ok((r, hw.reg_read(m.arg_at).unwrap_or(u32::MAX)))
}
