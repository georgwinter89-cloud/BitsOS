//! The HD Audio controller's registers (Intel HDA spec 1.0a §3.3; Linux `include/sound/hda_register.h`), with
//! the access width Linux uses for each: QEMU's intel-hda finds a register by its exact start address, so a
//! byte write into the middle of a register (e.g. `SD_CTL + 2`) is lost there.

/// PCI ids this driver binds.
pub const VENDOR_INTEL: u16 = 0x8086;
pub const VENDOR_AMD: u16 = 0x1022;
pub const VENDOR_ATI: u16 = 0x1002;
/// QEMU `-device intel-hda` (ICH6) and `-device ich9-intel-hda`.
pub const DEV_ICH6: u16 = 0x2668;
pub const DEV_ICH9: u16 = 0x293E;
/// The Mini-PC's FCH HD Audio at C7:00.6 (Linux `hda_intel.c:2567`, `AZX_DCAPS_PRESET_AMD_SB`).
pub const DEV_AMD_FCH: u16 = 0x15E3;
/// The Radeon 780M's HDMI/DP audio controller at C7:00.1 (Linux `hda_intel.c` ATI HDMI, `PRESET_ATI_HDMI_NS`).
pub const DEV_ATI_HDMI: u16 = 0x1640;

pub const GCAP: usize = 0x00; // 16
pub const VMIN: usize = 0x02; // 8
pub const VMAJ: usize = 0x03; // 8
pub const GCTL: usize = 0x08; // 32
pub const STATESTS: usize = 0x0E; // 16
pub const INTCTL: usize = 0x20; // 32
pub const INTSTS: usize = 0x24; // 32
pub const CORBLBASE: usize = 0x40; // 32
pub const CORBUBASE: usize = 0x44; // 32
pub const CORBWP: usize = 0x48; // 16
pub const CORBRP: usize = 0x4A; // 16
pub const CORBCTL: usize = 0x4C; // 8
pub const CORBSIZE: usize = 0x4E; // 8
pub const RIRBLBASE: usize = 0x50; // 32
pub const RIRBUBASE: usize = 0x54; // 32
pub const RIRBWP: usize = 0x58; // 16
pub const RINTCNT: usize = 0x5A; // 16
pub const RIRBCTL: usize = 0x5C; // 8
pub const RIRBSTS: usize = 0x5D; // 8
pub const RIRBSIZE: usize = 0x5E; // 8
pub const IC: usize = 0x60; // 32: immediate command
pub const IR: usize = 0x64; // 32: immediate response
pub const ICS: usize = 0x68; // 16: immediate command status
pub const DPLBASE: usize = 0x70; // 32
pub const DPUBASE: usize = 0x74; // 32

pub const GCTL_CRST: u32 = 1 << 0;
pub const GCTL_UNSOL: u32 = 1 << 8;
pub const INT_GLOBAL_EN: u32 = 1 << 31;
pub const INT_CTRL_EN: u32 = 1 << 30;
/// INTSTS/INTCTL bits 29:0: one per stream descriptor (index order: inputs, outputs, bidirectional).
pub const INT_ALL_STREAM: u32 = (1 << 30) - 1;
pub const STATESTS_MASK: u16 = 0x7FFF;
pub const CORBRP_RST: u16 = 1 << 15;
pub const CORBCTL_RUN: u8 = 1 << 1;
pub const RIRBWP_RST: u16 = 1 << 15;
pub const RIRBCTL_IRQ: u8 = 1 << 0;
pub const RIRBCTL_DMA: u8 = 1 << 1;
/// RIRBSTS: response interrupt (bit 0) and overrun (bit 2), both write-1-to-clear.
pub const RIRBSTS_MASK: u8 = 0x05;
pub const ICS_BUSY: u16 = 1 << 0;
pub const ICS_VALID: u16 = 1 << 1;
/// RIRB entry, second dword: bit 4 unsolicited, bits 3:0 the codec.
pub const RIRB_EX_UNSOL: u32 = 1 << 4;

/// Stream descriptor `i` (0-based over all descriptors) and its registers.
pub const SD_BASE: usize = 0x80;
pub const SD_LEN: usize = 0x20;
pub const fn sd(i: usize) -> usize {
    SD_BASE + i * SD_LEN
}
pub const SD_CTL: usize = 0x00; // 32 (bytes 0-2) and 8 (byte 0)
pub const SD_STS: usize = 0x03; // 8
pub const SD_LPIB: usize = 0x04; // 32
pub const SD_CBL: usize = 0x08; // 32
pub const SD_LVI: usize = 0x0C; // 16
pub const SD_FIFOS: usize = 0x10; // 16
pub const SD_FORMAT: usize = 0x12; // 16
pub const SD_BDLPL: usize = 0x18; // 32
pub const SD_BDLPU: usize = 0x1C; // 32

pub const SD_CTL_SRST: u8 = 1 << 0;
pub const SD_CTL_RUN: u8 = 1 << 1;
pub const SD_CTL_IOCE: u8 = 1 << 2;
pub const SD_CTL_FEIE: u8 = 1 << 3;
pub const SD_CTL_DEIE: u8 = 1 << 4;
/// The three interrupt enables (Linux `SD_INT_MASK` in the control byte).
pub const SD_CTL_INT: u8 = SD_CTL_IOCE | SD_CTL_FEIE | SD_CTL_DEIE;
/// Stream tag, bits 23:20 of the 32-bit control register.
pub const SD_CTL_TAG_SHIFT: u32 = 20;
pub const SD_CTL_TAG_MASK: u32 = 0xF << 20;
/// Traffic priority (Linux sets it when snooping is off).
pub const SD_CTL_TP: u32 = 1 << 18;
/// SD_STS: buffer completion (2), FIFO error (3), descriptor error (4); write 1 to clear. FIFO ready (5).
pub const SD_STS_BCIS: u8 = 1 << 2;
pub const SD_STS_FIFOE: u8 = 1 << 3;
pub const SD_STS_DESE: u8 = 1 << 4;
pub const SD_STS_MASK: u8 = 0x1C;

/// One buffer descriptor list entry: address (64), length (32), IOC flag (32).
pub const BDLE_LEN: usize = 16;
/// Buffers and the list must start on 128 bytes (HDA spec 3.6.2); periods are multiples of it (Linux's step).
pub const BDL_ALIGN: usize = 128;
/// LVI holds 8 bits: at most 256 entries, at least 2 (protocol facts).
pub const BDL_MAX: usize = 256;
pub const BDL_MIN: usize = 2;
/// Stream tags have 4 bits and 0 means "none": 15 per direction (protocol fact).
pub const TAG_MAX: u8 = 15;

/// Config space: Intel TCSEL (0x44, bits 2:0 cleared by Linux unless `NO_TCSEL`) and ATI's snoop byte (0x42).
pub const PCI_TCSEL: u8 = 0x44;
pub const PCI_ATI_MISC2: u8 = 0x42;
pub const ATI_ENABLE_SNOOP: u32 = 0x02;

/// GCAP fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gcap {
    pub oss: usize,
    pub iss: usize,
    pub bss: usize,
    pub ok64: bool,
}

impl Gcap {
    pub fn parse(v: u16) -> Gcap {
        Gcap { oss: (v >> 12 & 0xF) as usize, iss: (v >> 8 & 0xF) as usize, bss: (v >> 3 & 0x1F) as usize, ok64: v & 1 != 0 }
    }
    /// Descriptors in all.
    pub fn total(&self) -> usize {
        self.iss + self.oss + self.bss
    }
    /// Index of output descriptor `n` (inputs come first).
    pub fn output(&self, n: usize) -> usize {
        self.iss + n
    }
}
