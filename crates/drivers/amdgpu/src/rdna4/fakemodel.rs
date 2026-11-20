//! GP-GPU2: the RX 9070 XT as a model with the faces of PSP 14.0.3 and SMU 14.0.2, over the read-only attach's
//! `FakeCard` (memory size, ready bit, discovery table). It refuses, and keeps, everything Linux 7.3.1 would not do
//! at this point, so a test proves the driver's order and values instead of merely running them:
//!
//! - the bootloader (`fakeboot`): a cold card has no secure OS; Linux's nine loads, each as Linux does it, and
//!   nothing else; only after the ninth is there a secure OS; none of them before GMC and IH are started
//!   (Linux's order of blocks), and no buffer of the secure processor in the GART table or the default page;
//! - PSP (psp_v14_0.c, amdgpu_psp.c): ring create only on a live secure OS, after ready (bit 31, status 0) was
//!   read, with 69, 70, 71 written in that order since the last command, a 4 KiB ring inside VRAM; never a second
//!   ring; destroy only of a ring that exists; nothing but create and destroy in C2PMSG_64; no frame without a ring;
//!   the commands in Linux's order for 14.0.3 (`fakecmd`: the 31 of the trace), every address inside VRAM; never
//!   SETUP_TMR or DESTROY_TMR (the boot-time TMR), a flash, SR-IOV or debug register; never a write into the
//!   firmware's picture; a frame only after an HDP flush at Linux's remapped place, with the remap in force;
//! - SDMA (wave 43, `fakesdma` and `fakesdmaeng`): no register write and no doorbell without the GC block's
//!   power, no unhalt or ring before its firmware, a ring only on bound pages, a doorbell only as Linux rings it;
//!   the engines execute Linux's packets through the GART and stop at any other;
//! - SMU: the index pair only at the firmware flags' SMN address (never put back); the mailbox as answer 0,
//!   argument, message; questions at any time, the messages of Linux's start of the power controller only as
//!   Linux sends them (`fakepmfw`), none of the never-messages;
//! - the graphics block and its hub (`fakegc`, `fakegchub`): nothing before power and a complete autoload;
//! - the command processor (wave 44, `fakecp`, `fakecpeng`): the constants, the clear-state buffer and the
//!   unhalt only as Linux makes them; the queues MES mapped run Linux's GFX 12 packets through GFXHUB;
//! - MES (wave 44, `fakemes` and `fakemesapi`): its pipes only as Linux starts them, on firmware the secure
//!   processor loaded and on a command processor `gccp` resumed; its rings read through GFXHUB, each command as
//!   the header documents it and only Linux's; a kernel queue it maps is handed to the command processor's
//!   model from its MQD (`Model::cp_place_queue`), which refuses it on halted engines;
//! - the rings (wave 44, `fakering`): CP_MAX_CONTEXT, CP_DEVICE_ID and the interrupt enables only as Linux
//!   writes them, after the queues are mapped; a hung queue shows in GRBM_STATUS and CP_STAT.
//!
//! A refused write fails (`reg_write` false); a refused command is answered with a non-zero status, as a PSP that
//! does not know it would. The model answers reads as the real card answered Linux (the trace's values), so a
//! stage's trail can be held against the trace line by line.

use super::fakeboot::{Boot, C64_READY, SOL};
use super::fakecard::FakeCard;
use super::fakecmd::Expect;
use super::regs14::Regs14;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;
use rdna4_core::regmap::Reg;

/// VRAM's MC base as the journal of 11.11. shows it (0x8000000000).
pub const MC_BASE: u64 = 0x80_0000_0000;
/// The firmware's reserved region as the secure OS names it: Linux frees the VBIOS's 516 MiB reservation and
/// reserves 50 MiB and 4 MiB instead right after the two questions (the trace's buffer events, init.txt lines
/// 766-770), and takes the first only when it ends at the top of VRAM (amdgpu_psp.c:1099-1102). Where the
/// extended one lies the trace does not say: assumed right below the first.
pub const RESERVED: u32 = 50 << 20;
pub const RESERVED_EXT: u32 = 4 << 20;
pub const SMU_VERSION: u32 = 0x0068_5000;
pub const SMU_IF: u32 = 0x33;
pub const TMR_SIZE: u32 = 0x0130_0000;
/// The SOS descriptor's version of `fakefiles::sos` (the real file's, 0x3A1214).
pub const SOS_VERSION: u32 = 0x003A_1214;
/// Each clock's (min, max) in MHz, distinct so a wrong clock id shows (invented values).
pub const CLOCKS: [(u32, u32, u32); 3] = [(0, 500, 2970), (1, 600, 1200), (2, 96, 1258)];

pub struct Model {
    pub card: FakeCard,
    pub regs: Regs14,
    pages: BTreeMap<u64, Box<[u8; 4096]>>,
    /// The firmware's picture (VRAM offsets): never written.
    pub picture: Range<u64>,
    /// What Linux would not have done, in words.
    pub refused: Vec<String>,
    /// Every register write that reached the model.
    pub writes: Vec<(u32, u32)>,
    // PSP face.
    pub sol: u32,
    pub c64: u32,
    pub boot: Boot,
    pub c67: u32,
    pub c69: u32,
    pub c70: u32,
    pub c71: u32,
    pub(super) staged: Vec<u32>,
    pub(super) ready_seen: bool,
    /// (MC, bytes) of the ring the PSP uses; a ring from before when built with `stale_ring`.
    pub ring: Option<(u64, u32)>,
    /// How many commands of Linux's sequence this ring answered (`fakecmd::Model::script`).
    pub done: usize,
    pub(super) slot: u32,
    /// What Linux hands the secure OS: the TOC, the images, the applications.
    pub expect: Expect,
    pub commands: Vec<u32>,
    /// VRAM written since the last HDP flush (bytes the card may not see yet), and the flushes counted.
    pub(super) unflushed: bool,
    pub flushes: u32,
    /// The firmware's reserved region at the top of VRAM and its extension below it (bytes); `reserved_at` puts
    /// the first elsewhere and `reserved_ext_at` the extension (tests of a region that reaches into the window or
    /// over the GART table).
    pub reserved: u32,
    pub reserved_ext: u32,
    pub reserved_at: Option<u64>,
    pub reserved_ext_at: Option<u64>,
    /// Tests: the trusted application with this index (0 RAS, 1 HDCP, 2 DTM) is answered with this status.
    pub ta_status: Option<(usize, u32)>,
    /// Tests: no fence for this command id (a PSP that hangs on it).
    pub hang_on: Option<u32>,
    /// Tests: DESTROY_RINGS answered with this status, the ring kept up.
    pub destroy_status: Option<u16>,
    /// Tests: ring create is taken, but its answer comes only after the driver stopped waiting.
    pub create_late: bool,
    /// The secure OS's version the driver must read from the stick: below 0x3A0E14 Linux does not ask for the
    /// reserved region (amdgpu_psp.c:1083-1086), so the model refuses the questions then.
    pub sos_version: u32,
    // SMU face.
    pub index: u32,
    pub smu_flags: u32,
    pub c90: u32,
    pub c82: u32,
    pub(super) smu_cleared: bool,
    pub(super) smu_arg_set: bool,
    pub messages: Vec<(u16, u32)>,
    /// Tests: this message is never answered (the answer register stays 0).
    pub smu_silent_on: Option<u16>,
    /// Wave 43: the power controller as Linux's start of it meets it (`fakepmfw`). The blocks' parts are boxed:
    /// the model is moved by value through every stage call (into the guard, the register owner and back), so
    /// each of its bytes costs a test's stack several times over (the release ktests hold a test to 48 KiB).
    pub pmfw: Box<super::fakepmfw::FakePmfw>,
    // GC after the autoload: the two registers of Linux's wait, and the block itself (`fakegc`).
    pub cp_stat: u32,
    pub bootload: u32,
    pub gc: Box<super::fakegc::FakeGc>,
    /// Wave 44: the graphics block's command processor (`fakecp`, `fakecpeng`).
    pub cp: Box<super::fakecp::FakeCp>,
    /// Wave 42: NBIF, MMHUB, IH and the host side (`fakenb`).
    pub nb: Box<super::fakenb::FakeNb>,
    /// Wave 43: SDMA's two engines (`fakesdma`).
    pub sdma: Box<super::fakesdma::FakeSdma>,
    /// Wave 44: MES (`fakemes`, `fakemesapi`).
    pub mes: Box<super::fakemes::FakeMes>,
    /// Wave 44: the rings' registers (`fakering`).
    pub rings: Box<super::fakering::FakeRing>,
    /// The lowest stack address (`stack_at`) at which a register or memory access reached the model: how deep the
    /// driver's frames were when it touched the card. A stage runs on MCP's 64 KiB thread on the real machine.
    pub deepest: usize,
}

/// How far below its caller a stage may be when it reaches the card (`Model::deepest`). On the real machine a
/// stage runs on MCP's 64 KiB thread, and the gate gives a whole test 48 KiB of it (`ktest_stack`). Measured
/// 19.11.: 13 KiB at the deepest (`ring`), release and debug kernel alike, so this leaves a new stage 11 KiB of
/// frames and still stops a buffer held on the stack (wave 42's `gmc` held a 64 KiB piece of the GART table there).
pub const STAGE_STACK: usize = 24 << 10;

pub use super::depth::stack_at;

pub(super) fn same(a: Reg, b: Reg) -> bool {
    a.hw_id == b.hw_id && a.base_index == b.base_index && a.offset == b.offset
}

impl Model {
    /// The measured card as Linux meets it at a cold start: no secure OS, the bootloader ready, the HDP flush not
    /// remapped, no GART, the IH off (the three blocks before the secure processor are `fakenb`'s); its picture
    /// at the start of VRAM (2560x1440, 4 bytes a pixel). `boot` are Linux's nine bootloader loads, `expect`
    /// what the ring gets.
    pub fn new(boot: Vec<(u32, Vec<u8>)>, expect: Expect) -> Model {
        let card = FakeCard::measured();
        let d = crate::gpucore::disc::parse(&card.blob).expect("the measured blob parses");
        let regs = Regs14::from(&d).expect("the measured table names every register");
        let n = super::nbregs::NbRegs::from(&d, &regs).expect("the measured table names every register");
        let gc = Box::new(super::fakegc::FakeGc::new(&n));
        let cp = Box::new(super::fakecp::FakeCp::new(&n, super::cpstage::config(&d).expect("the measured table has GC info")));
        let nb = Box::new(super::fakenb::FakeNb::new(n));
        let sdma = Box::new(super::fakesdma::FakeSdma::new(super::sdmaregs::SdmaRegs::from(&d).expect("the measured table names every register")));
        let mes = Box::new(super::fakemes::FakeMes::new(&d));
        Model {
            mes,
            rings: Box::default(),
            deepest: usize::MAX,
            sdma,
            nb,
            gc,
            cp,
            card,
            regs,
            pages: BTreeMap::new(),
            picture: 0..2560 * 1440 * 4,
            refused: Vec::new(),
            writes: Vec::new(),
            sol: 0,
            c64: 0,
            boot: Boot::new(boot),
            c67: 0,
            c69: 0,
            c70: 0,
            c71: 0,
            staged: Vec::new(),
            ready_seen: false,
            ring: None,
            done: 0,
            slot: 0,
            expect,
            commands: Vec::new(),
            unflushed: false,
            flushes: 0,
            reserved: RESERVED,
            reserved_ext: RESERVED_EXT,
            reserved_at: None,
            reserved_ext_at: None,
            ta_status: None,
            hang_on: None,
            destroy_status: None,
            create_late: false,
            sos_version: SOS_VERSION,
            index: 0x1234_0000,
            smu_flags: 1,
            c90: 1,
            c82: 0,
            smu_cleared: false,
            smu_arg_set: false,
            messages: Vec::new(),
            smu_silent_on: None,
            pmfw: Box::new(super::fakepmfw::FakePmfw::new()),
            cp_stat: 0x8000_0000,
            bootload: 0,
        }
    }

    /// The card with its secure OS running already (a kernel before this one loaded it): the bootloader is past,
    /// and the HDP flush is where that kernel's `nbif` remapped it.
    pub fn warm(mut self) -> Model {
        (self.sol, self.c64) = (SOL, C64_READY);
        self.boot.taken = self.boot.parts.len();
        self.nb.left_remapped();
        self
    }

    /// A ring an earlier kernel left (its registers still set), on a secure OS that runs.
    pub fn stale_ring(self) -> Model {
        let mut m = self.warm();
        let mc = MC_BASE + (100 << 20);
        m.ring = Some((mc, 0x1000));
        (m.c69, m.c70, m.c71, m.c67) = (mc as u32, (mc >> 32) as u32, 0x1000, 0x20);
        m
    }

    pub fn vram_bytes(&self) -> u64 {
        u64::from(self.card.memsize) << 20
    }

    pub(super) fn in_vram(&self, mc: u64, bytes: u64) -> bool {
        mc >= MC_BASE && mc.checked_add(bytes).is_some_and(|e| e <= MC_BASE + self.vram_bytes())
    }

    pub(super) fn refuse(&mut self, what: String) -> bool {
        self.refused.push(what);
        false
    }

    pub fn vram_peek(&self, off: u64, buf: &mut [u8]) {
        self.peek(off, buf)
    }

    /// An HDP flush arrived (through whichever register Linux flushes at this point).
    pub(super) fn hdp_flushed(&mut self) {
        (self.unflushed, self.flushes) = (false, self.flushes + 1);
        self.nb.table_flushed();
        self.gc.table_flushed();
        self.sdma_hdp_flushed();
    }

    /// A 4 KiB page of VRAM as it stands (`None`: never written, all zero).
    pub(super) fn vram_page(&self, page: u64) -> Option<&[u8; 4096]> {
        self.pages.get(&page).map(|p| &**p)
    }

    /// Page by page: megabytes are read and written here (the firmware buffer).
    pub(super) fn peek(&self, off: u64, buf: &mut [u8]) {
        let mut done = 0;
        while done < buf.len() {
            let a = off + done as u64;
            let (page, at) = (a >> 12, (a & 0xFFF) as usize);
            let n = (4096 - at).min(buf.len() - done);
            match self.pages.get(&page) {
                Some(p) => buf[done..done + n].copy_from_slice(&p[at..at + n]),
                None => buf[done..done + n].fill(0),
            }
            done += n;
        }
    }

    pub(super) fn poke(&mut self, off: u64, data: &[u8]) {
        let mut done = 0;
        while done < data.len() {
            let a = off + done as u64;
            let (page, at) = (a >> 12, (a & 0xFFF) as usize);
            let n = (4096 - at).min(data.len() - done);
            let src = &data[done..done + n];
            // A page of zeros that was never written stays unwritten (a megabyte of zeros costs nothing).
            if self.pages.contains_key(&page) || src.iter().any(|b| *b != 0) {
                self.pages.entry(page).or_insert_with(|| Box::new([0; 4096]))[at..at + n].copy_from_slice(src);
            }
            done += n;
        }
    }

    pub(super) fn word(&self, mc: u64) -> u32 {
        let mut b = [0u8; 4];
        self.peek(mc - MC_BASE, &mut b);
        u32::from_le_bytes(b)
    }

    pub(super) fn put_word(&mut self, mc: u64, v: u32) {
        self.poke(mc - MC_BASE, &v.to_le_bytes());
    }

    pub(super) fn reg_of(&self, byte: u32) -> Option<Reg> {
        self.regs.reg_at(byte)
    }
}
