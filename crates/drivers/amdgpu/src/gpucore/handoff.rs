//! GX-7f: what the PSP/GC bring-up left for the next kernel (a Reload's note), pure and host-tested.
//!
//! After `DESTROY_RINGS` the PSP keeps C2PMSG_67 (write pointer, 0x10 after one frame) and C2PMSG_71 (ring size)
//! as they were (the Mini-PC 28.10.), so a clean stop and a crash look the same in the registers. The Quiesce
//! hook therefore hands a small note on through the Reload (kexec's crash page, `psplive::NoteIo`): what it did
//! to ring and TMR, what GC was left in, the PSP registers it saw afterwards (the next kernel trusts the note only
//! when they still read the same), and whether bus master is ours and still on.
//!
//! Layout (32 bytes, little endian): 0 magic "GXSTOP01", 8 ring fate u8, 9 GC fate u8 (3: unloaded, GX-7j3), 10 flags u8 (bit 0 TMR
//! destroyed, bit 1 bus master switched on by BitsOS and still on), 11 zero, 12 C2PMSG_67 u32, 16 C2PMSG_71 u32,
//! 20 C2PMSG_69 u32, 24 C2PMSG_70 u32, 28 FNV-1a 32 over bytes 0..28.

/// The note's tag in kexec's driver-note slot.
pub const TAG: u32 = u32::from_le_bytes(*b"GXPS");
pub const MAGIC: [u8; 8] = *b"GXSTOP01";
pub const LEN: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingFate {
    /// No ring was made by the kernel that wrote the note (and none it inherited was left).
    None,
    /// DESTROY_RINGS answered and the PSP holds nothing.
    Destroyed,
    /// Left up (kept for GC, a run in progress, a failed stop).
    NotDestroyed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GcFate {
    /// No EnableGfxImu was sent.
    Untouched,
    /// The GC stage booted GC (CP idle, bootload complete).
    Booted,
    /// EnableGfxImu reached PMFW and GC did not boot: only a reboot clears it (mode 2 is refused).
    Unsettled,
    /// GX-7j3: GC booted, then the stop unloaded it as Linux does (PrepareMp1ForUnload answered, RLC stopped)
    /// before the TMR went. A note saying `Booted` after a stop is one without that (an older kernel's).
    Unloaded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StopNote {
    pub ring: RingFate,
    pub gc: GcFate,
    pub tmr_destroyed: bool,
    pub bus_master_left_on: bool,
    /// C2PMSG_67, 71, 69, 70 as read after the stop (`u32::MAX`: not read).
    pub wptr: u32,
    pub size: u32,
    pub addr_lo: u32,
    pub addr_hi: u32,
}

fn fnv32(b: &[u8]) -> u32 {
    b.iter().fold(0x811c_9dc5u32, |h, &x| (h ^ u32::from(x)).wrapping_mul(0x0100_0193))
}

impl StopNote {
    pub fn bytes(&self) -> [u8; LEN] {
        let mut b = [0u8; LEN];
        b[..8].copy_from_slice(&MAGIC);
        b[8] = match self.ring {
            RingFate::None => 0,
            RingFate::Destroyed => 1,
            RingFate::NotDestroyed => 2,
        };
        b[9] = match self.gc {
            GcFate::Untouched => 0,
            GcFate::Booted => 1,
            GcFate::Unsettled => 2,
            GcFate::Unloaded => 3,
        };
        b[10] = u8::from(self.tmr_destroyed) | u8::from(self.bus_master_left_on) << 1;
        for (i, v) in [self.wptr, self.size, self.addr_lo, self.addr_hi].into_iter().enumerate() {
            b[12 + 4 * i..16 + 4 * i].copy_from_slice(&v.to_le_bytes());
        }
        let c = fnv32(&b[..28]);
        b[28..].copy_from_slice(&c.to_le_bytes());
        b
    }

    /// `None` for anything that is not a whole, intact note of this version.
    pub fn parse(b: &[u8]) -> Option<StopNote> {
        let b: &[u8; LEN] = b.get(..LEN)?.try_into().ok()?;
        if b[..8] != MAGIC || b[11] != 0 || b[10] & !3 != 0 || u32::from_le_bytes([b[28], b[29], b[30], b[31]]) != fnv32(&b[..28]) {
            return None;
        }
        let w = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        Some(StopNote {
            ring: match b[8] {
                0 => RingFate::None,
                1 => RingFate::Destroyed,
                2 => RingFate::NotDestroyed,
                _ => return None,
            },
            gc: match b[9] {
                0 => GcFate::Untouched,
                1 => GcFate::Booted,
                2 => GcFate::Unsettled,
                3 => GcFate::Unloaded,
                _ => return None,
            },
            tmr_destroyed: b[10] & 1 != 0,
            bus_master_left_on: b[10] & 2 != 0,
            wptr: w(12),
            size: w(16),
            addr_lo: w(20),
            addr_hi: w(24),
        })
    }

    /// Whether the PSP registers read now are the ones the note saw (otherwise something wrote them since).
    pub fn matches(&self, wptr: Option<u32>, size: Option<u32>, lo: Option<u32>, hi: Option<u32>) -> bool {
        wptr == Some(self.wptr) && size == Some(self.size) && lo == Some(self.addr_lo) && hi == Some(self.addr_hi)
    }

    pub fn text(&self) -> alloc::string::String {
        alloc::format!(
            "ring {:?}, TMR {}, GC {:?}, bus master {}, C2PMSG_67 {:#x} C2PMSG_71 {:#x} C2PMSG_69 {:#x} C2PMSG_70 {:#x}",
            self.ring,
            if self.tmr_destroyed { "destroyed" } else { "not destroyed" },
            self.gc,
            if self.bus_master_left_on { "left on by BitsOS" } else { "not left on" },
            self.wptr,
            self.size,
            self.addr_lo,
            self.addr_hi
        )
    }
}

/// The note as the PSP registers read now (`u32::MAX`: unreadable), for what a run, a stop or the Quiesce did.
pub fn read_note(hw: &mut dyn super::psp::Hw, a: &crate::ipmod::psp13::Addrs, ring: RingFate, gc: GcFate, tmr_destroyed: bool, bus_master_left_on: bool) -> StopNote {
    let [wptr, size, addr_lo, addr_hi] = [a.c2pmsg_67, a.c2pmsg_71, a.c2pmsg_69, a.c2pmsg_70].map(|b| hw.reg_read(b).unwrap_or(u32::MAX));
    StopNote { ring, gc, tmr_destroyed, bus_master_left_on, wptr, size, addr_lo, addr_hi }
}

/// How the note crosses a Reload: kexec's driver-note slot (a change request on RB, gx-GX-7f.md), bound by the
/// kernel at boot. `put` runs in the Quiesce hook (interrupts off: no allocation, no lock); `take` gives the note
/// the kernel before handed on in this Reload (nothing after a firmware start); `from_reload` tells whether this
/// boot came from a Reload at all (kexec's boot marker of the same boot), so "nothing handed on" can say which.
#[derive(Clone, Copy)]
pub struct NoteIo {
    pub put: fn(u32, &[u8]) -> bool,
    pub take: fn(u32) -> Option<alloc::vec::Vec<u8>>,
    pub from_reload: fn() -> bool,
}

impl NoteIo {
    pub fn put_note(&self, n: &StopNote) -> bool {
        (self.put)(TAG, &n.bytes())
    }
    /// `None` when nothing was handed on or the bytes are not an intact note.
    pub fn take_note(&self) -> Option<StopNote> {
        (self.take)(TAG).and_then(|b| StopNote::parse(&b))
    }
}

/// GX-7i2: how this boot's note from the kernel before arrived (logged once at warn level, shown by gx_probe, so
/// the desk sees it at any loglevel).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrival {
    Inherited(StopNote),
    /// The kernel bound no way for the note (`NoteIo`): nothing can be handed on or read.
    Unbound,
    /// Nothing handed on after a firmware start (or a reset that kept RAM).
    Nothing,
    /// A Reload, but the kernel before handed no note (it never ran its Quiesce hook, or the Reload's crash page
    /// did not carry it).
    ReloadWithoutNote,
    /// Bytes handed on under the note's tag that are not an intact note of this version.
    Broken { len: usize },
}

impl Arrival {
    pub fn of(io: Option<&NoteIo>) -> Arrival {
        let Some(io) = io else { return Arrival::Unbound };
        match (io.take)(TAG) {
            None if (io.from_reload)() => Arrival::ReloadWithoutNote,
            None => Arrival::Nothing,
            Some(b) => StopNote::parse(&b).map_or(Arrival::Broken { len: b.len() }, Arrival::Inherited),
        }
    }

    pub fn note(&self) -> Option<StopNote> {
        match self {
            Arrival::Inherited(n) => Some(*n),
            _ => None,
        }
    }

    /// The reason in words (the `note.none` line and gx_probe's).
    pub fn text(&self) -> alloc::string::String {
        match self {
            Arrival::Inherited(n) => alloc::format!("inherited: {}", n.text()),
            Arrival::Unbound => alloc::string::String::from("none: this kernel bound no way for the note (no Reload hand-over)"),
            Arrival::Nothing => alloc::string::String::from("none: nothing handed on (a firmware start)"),
            Arrival::ReloadWithoutNote => alloc::string::String::from("none: nothing handed on (a Reload, but the kernel before handed no note)"),
            Arrival::Broken { len } => alloc::format!("none: {len} bytes handed on under the tag GXPS are not an intact note of this version (ignored)"),
        }
    }
}

/// What a Quiesce that found no ring hands on: this boot's last note (bus master as now), else the inherited one
/// unchanged, so a chain of Reloads without runs keeps it. Its bus master flag is replaced only when this boot took
/// that bus master over (`adopted`); otherwise the kernel before's "left on" stays true for the next kernel.
pub fn idle_hand_on(last: Option<StopNote>, inherited: Option<StopNote>, adopted: bool, bus_master_ours: bool) -> Option<StopNote> {
    match (last, inherited) {
        (Some(n), _) => Some(StopNote { bus_master_left_on: bus_master_ours, ..n }),
        (None, Some(n)) if adopted => Some(StopNote { bus_master_left_on: bus_master_ours, ..n }),
        (None, n) => n,
    }
}

/// GX-7j3: the SMU and GC registers of a stop's unload (from the run that booted GC).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnloadAddrs {
    pub smu: crate::ipmod::smu13::Addrs,
    pub gc: super::gfxup::GcAddrs,
}

/// GX-7j3: a stop's unload of a booted GC (before the PSP's destroy): `Unloaded` when PMFW took it, `Booted`
/// otherwise (the next kernel then catches it up); any other fate is left as it is (nothing sent).
pub fn unload_gc(hw: &mut dyn super::psp::Hw, gc: GcFate, un: Option<&UnloadAddrs>, t: super::smu::SmuTimeouts, quiet: bool) -> (GcFate, Option<super::gfxup::Unload>) {
    match (gc, un) {
        (GcFate::Booted, Some(u)) => {
            let r = if quiet { super::gfxup::unload_quiet(hw, &u.smu, Some(&u.gc), t) } else { super::gfxup::unload(hw, &u.smu, Some(&u.gc), t) };
            (if r.told() { GcFate::Unloaded } else { GcFate::Booted }, Some(r))
        }
        _ => (gc, None),
    }
}

/// What the Quiesce hook does with a kept ring (the ktests run the same): a GC left unsettled keeps TMR and ring
/// (`None`: nothing destroyed under a possibly running RLC/IMU); otherwise (GX-7j3) a booted GC is unloaded first
/// (`unload_gc`, the Quiesce's SMU bounds), then both are destroyed with the Quiesce bounds (`Some(quiet)`: the PSP
/// provably holds nothing). The note says what the next kernel will find.
pub fn quiesce_kept<M: super::psp::Mailbox>(psp: &mut super::psp::Psp<M>, hw: &mut dyn super::psp::Hw, a: &crate::ipmod::psp13::Addrs, gc: GcFate, un: Option<&UnloadAddrs>, bus_master_on: bool) -> (Option<bool>, StopNote) {
    if gc == GcFate::Unsettled {
        return (None, read_note(hw, a, RingFate::NotDestroyed, GcFate::Unsettled, false, bus_master_on));
    }
    let (gc, _) = unload_gc(hw, gc, un, super::smu::SmuTimeouts::QUIESCE, true);
    psp.t = super::psp::Timeouts::QUIESCE;
    let r = psp.stop(hw);
    let quiet = r.is_ok() && psp.quiet();
    let ring = if quiet { RingFate::Destroyed } else { RingFate::NotDestroyed };
    (Some(quiet), read_note(hw, a, ring, gc, r.is_ok(), bus_master_on))
}
