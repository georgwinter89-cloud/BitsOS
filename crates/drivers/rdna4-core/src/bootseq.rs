//! The one way to the secure processor's bootloader: Linux's nine loads of a cold start, in Linux's order
//! (`psp_hw_start`, A:amdgpu_psp.c:2952-3048, through A:psp_v14_0.c:127-237), and nothing else. The register trace of
//! Linux's own cold start on this card shows exactly these nine (plan/hw/gaming-pc-gpu-2026-11-18, init.txt lines
//! 673-751): C2PMSG_81 reads 0 there, so without them the card has no secure OS, no ring and no firmware.
//!
//! `refuse::write_refused` refuses the bootloader's two registers for every value. A `Gate` opens them one step at a
//! time, as Linux takes them (`psp_v14_0_bootloader_load_component`): the sign of life read as 0, the bootloader's
//! ready bit read, the buffer's address into C2PMSG_36, the next command of the sequence into C2PMSG_35. A value
//! outside the nine, a command out of order or a second time, an address outside the window, and anything once
//! C2PMSG_81 read non-zero stays refused, and so does everything after a Reload that follows a kernel which sent
//! (`bootnote` carries that across). Nothing here sends anything.

use core::ops::Range;

/// A part of `psp_14_0_3_sos.bin` the bootloader takes. The number is `enum psp_fw_type` (A:amdgpu_ucode.h:118-133),
/// the descriptor's type in the file's documented header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Sos = 1,
    SysDrv = 2,
    Kdb = 3,
    Spl = 5,
    SocDrv = 7,
    IntfDrv = 8,
    DbgDrv = 9,
    RasDrv = 10,
    IpKeyMgr = 11,
}

impl Part {
    pub const fn name(self) -> &'static str {
        match self {
            Part::Kdb => "key database",
            Part::Spl => "SPL table",
            Part::SysDrv => "system driver",
            Part::SocDrv => "SOC driver",
            Part::IntfDrv => "interface driver",
            Part::DbgDrv => "debug driver",
            Part::RasDrv => "RAS driver",
            Part::IpKeyMgr => "IP key manager",
            Part::Sos => "secure OS",
        }
    }
}

/// Linux's order with each command (`enum psp_bootloader_cmd`, A:amdgpu_psp.h:95-109). The debug driver goes as
/// `PSP_BL__LOAD_HADDRV`, which is `LOAD_DBGDRV`'s value (A:psp_v14_0.c:185-189). SPDM is not loaded: 14.0's function
/// table has no `bootloader_load_spdm_drv` (A:psp_v14_0.c:679-700).
pub const SEQUENCE: [(Part, u32); 9] = [
    (Part::Kdb, 0x0008_0000),
    (Part::Spl, 0x1000_0000),
    (Part::SysDrv, 0x0001_0000),
    (Part::SocDrv, 0x000B_0000),
    (Part::IntfDrv, 0x000D_0000),
    (Part::DbgDrv, 0x000C_0000),
    (Part::RasDrv, 0x000E_0000),
    (Part::IpKeyMgr, 0x000F_0000),
    (Part::Sos, 0x0002_0000),
];
// The stage's last load is the secure OS's own (its wait is another): the sequence must end with it.
const _: () = assert!(matches!(SEQUENCE[8].0, Part::Sos));

/// The buffer the bootloader reads a part from: 1 MiB, 1 MiB aligned (`PSP_1_MEG`, A:amdgpu_psp.h:39;
/// `psp_sw_init`, A:amdgpu_psp.c:539-548). C2PMSG_36 carries its address in MiB.
pub const BUFFER_BYTES: u64 = 1 << 20;
pub const ADDRESS_SHIFT: u32 = 20;
/// `psp_v14_0_wait_for_bootloader`'s bit (A:psp_v14_0.c:113-121).
pub const BOOTLOADER_READY: u32 = 0x8000_0000;

/// The word for C2PMSG_36 (`fw_pri_mc_addr >> 20`, A:psp_v14_0.c:147-148), or why this address gets none: it must
/// be 1 MiB aligned (the shift would drop the rest and the bootloader would read from another place), fit the
/// register, and lie with its megabyte inside `window`.
pub fn address_word(mc: u64, window: &Range<u64>) -> Result<u32, &'static str> {
    if !mc.is_multiple_of(BUFFER_BYTES) {
        return Err("the firmware buffer is not 1 MiB aligned");
    }
    let end = mc.checked_add(BUFFER_BYTES).ok_or("the firmware buffer's address overflows")?;
    if mc < window.start || end > window.end {
        return Err("the firmware buffer lies outside the window");
    }
    u32::try_from(mc >> ADDRESS_SHIFT).map_err(|_| "the firmware buffer's address does not fit C2PMSG_36")
}

/// Which of the bootloader's two registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mailbox {
    /// C2PMSG_35.
    Command,
    /// C2PMSG_36.
    Address,
}

/// Linux's steps for one part, in order. A read moves the gate on; a write passes only at its own step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Nothing of this part yet: C2PMSG_81 must be read (and be 0) first.
    Start,
    /// The secure OS was seen not running: the bootloader's ready bit must be read next.
    NotAlive,
    /// The bootloader was seen ready: the address may go.
    Ready,
    /// The address went: the command may go.
    Addressed,
}

/// The state of a card's bootloader as its driver saw it in this boot. One per card and boot (it must outlive a
/// stage: a command is never sent twice).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gate {
    /// MC range the firmware buffer must lie in.
    window: Range<u64>,
    /// How many of `SEQUENCE` went out.
    sent: usize,
    step: Step,
    /// C2PMSG_81 read non-zero once: closed for good.
    alive: bool,
    /// A kernel before this Reload sent to this bootloader in the same power cycle (`bootnote`): closed for good.
    spent: bool,
}

impl Gate {
    pub fn new(window: Range<u64>) -> Gate {
        Gate { window, sent: 0, step: Step::Start, alive: false, spent: false }
    }

    /// The gate of a bootloader that already got commands from the kernel before this Reload: it lets nothing
    /// through. What the bootloader does with a part it already has is unknown, so only a card that was off starts
    /// at the key database again.
    pub fn spent(window: Range<u64>) -> Gate {
        Gate { spent: true, ..Gate::new(window) }
    }

    /// The part whose command is next, `None` after the ninth.
    pub fn next(&self) -> Option<(Part, u32)> {
        SEQUENCE.get(self.sent).copied()
    }

    pub fn sent(&self) -> usize {
        self.sent
    }

    pub fn alive(&self) -> bool {
        self.alive
    }

    /// A read of C2PMSG_81. All ones (a card that does not answer) counts as alive: nothing is sent to it.
    pub fn saw_sign_of_life(&mut self, value: u32) {
        if value != 0 {
            self.alive = true;
        } else if self.step == Step::Start {
            self.step = Step::NotAlive;
        }
    }

    /// A read of C2PMSG_35.
    pub fn saw_bootloader(&mut self, value: u32) {
        let ready = value != u32::MAX && value & BOOTLOADER_READY != 0;
        if ready && self.step == Step::NotAlive {
            self.step = Step::Ready;
        }
    }

    /// Why this write is refused now, or `None`: it is exactly Linux's next step.
    pub fn refused(&self, to: Mailbox, value: u32) -> Option<&'static str> {
        if self.alive {
            return Some("the secure OS runs: nothing goes to the bootloader any more");
        }
        if self.spent {
            return Some("the kernel before this Reload sent to this bootloader: nothing more until the card was off");
        }
        let Some((_, command)) = self.next() else {
            return Some("all nine bootloader commands went out in this boot");
        };
        match to {
            Mailbox::Address => {
                if self.step != Step::Ready {
                    return Some(match self.step {
                        Step::Start => "the bootloader's address before C2PMSG_81 was read as 0",
                        Step::Addressed => "the bootloader's address a second time",
                        _ => "the bootloader's address before its ready bit was read",
                    });
                }
                match address_word(u64::from(value) << ADDRESS_SHIFT, &self.window) {
                    Ok(_) => None,
                    Err(why) => Some(why),
                }
            }
            Mailbox::Command => {
                if value != command {
                    return Some(command_refused(value, self.sent));
                }
                match self.step {
                    Step::Addressed => None,
                    Step::Ready => Some("a bootloader command before its buffer's address"),
                    Step::NotAlive => Some("a bootloader command before the bootloader's ready bit was read"),
                    Step::Start => Some("a bootloader command before C2PMSG_81 was read as 0"),
                }
            }
        }
    }

    /// A write `refused` let through reached the card.
    pub fn wrote(&mut self, to: Mailbox) {
        match to {
            Mailbox::Address => self.step = Step::Addressed,
            Mailbox::Command => {
                self.sent += 1;
                self.step = Step::Start;
            }
        }
    }
}

/// Why a command value that is not the next of the sequence is refused, by what it is.
fn command_refused(value: u32, sent: usize) -> &'static str {
    match SEQUENCE.iter().position(|(_, c)| *c == value) {
        Some(i) if i < sent => "this bootloader command went out already",
        Some(_) => "a bootloader command out of Linux's order",
        None => match value {
            0x0010_0000 | 0x0020_0000 => "memory training is never sent (Linux did not train this card)",
            0x2000_0000 => "the SPDM driver is not loaded by PSP 14.0",
            0x0200_0000 => "the USB-PD firmware version command",
            v if v >> 16 == 0x480 => "the USB-PD firmware load",
            _ => "not one of Linux's nine bootloader commands",
        },
    }
}
