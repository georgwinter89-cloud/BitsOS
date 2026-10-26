//! Why the display engine is refused: V1's reasons with their gate numbers (spec/gpu.md §1.2).

/// Why the display engine is not used: V1's reasons (`dcnsay.asm:187-202`), each with its gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// Gate 1: no AMD display function whose BAR0 holds the GOP framebuffer (or no GOP framebuffer at all).
    NotGop,
    /// Gate 2: memory decode off.
    Decode,
    /// Gate 2: BAR5 is not a non-zero 32-bit memory BAR.
    Bar5,
    /// Gate 2: a bus between the GPU and bus 0 has no bridge.
    Bridge,
    /// Gate 3: BAR5 can not be mapped (lies in RAM, or the mapper refused).
    Map,
    /// Gate 3b (V2): the codec ID through segment 2 (0xE318 == 0x1002AA01) is wrong.
    Codec,
    /// Gate 4.
    MemSize,
    Mode,
    Past,
    Aper,
    /// Gate 5.
    Dmcub,
    /// Gate 6.
    NoPipe,
    OtgOff,
    /// Gate 7.
    Held,
    Surf,
    Sign,
    /// A register the gates need could not be read (refused offset): never on the Mini-PC's BAR5.
    Read,
}

impl Why {
    pub const fn gate(self) -> u8 {
        match self {
            Why::NotGop => 1,
            Why::Decode | Why::Bar5 | Why::Bridge => 2,
            Why::Map | Why::Codec => 3,
            Why::MemSize | Why::Mode | Why::Past | Why::Aper | Why::Read => 4,
            Why::Dmcub => 5,
            Why::NoPipe | Why::OtgOff => 6,
            Why::Held | Why::Surf | Why::Sign => 7,
        }
    }
    pub const fn text(self) -> &'static str {
        match self {
            Why::NotGop => "no AMD display controller holds the GOP framebuffer",
            Why::Decode => "its memory space is off",
            Why::Bar5 => "BAR5 is no 32-bit memory BAR",
            Why::Bridge => "no bridge leads to its bus",
            Why::Map => "its registers cannot be mapped",
            Why::Codec => "the codec ID through segment 2 is not 1002AA01",
            Why::MemSize => "graphics memory of no believable size",
            Why::Mode => "the GOP mode is larger than 32 MB",
            Why::Past => "A1 would lie past the graphics memory",
            Why::Aper => "BAR0 or a bridge above it does not reach A1",
            Why::Dmcub => "the display's microcontroller runs with memory where A1 would lie",
            Why::NoPipe => "no pipe shows the GOP framebuffer",
            Why::OtgOff => "the timing generator of its pipe is off",
            Why::Held => "a flip on its pipe is held (lock, skip, swap group, triple buffer or master lock)",
            Why::Surf => "its pipe reads through page tables, encrypted or compressed",
            Why::Sign => "no DP clock signature for its timing generator",
            Why::Read => "a register the gates need could not be read",
        }
    }
}

/// A refusal: the reason and, where one register decided it, that register and its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub why: Why,
    pub reg: Option<(u32, u32)>,
}

impl Refusal {
    pub(crate) const fn new(why: Why) -> Self {
        Refusal { why, reg: None }
    }
    pub(crate) const fn at(why: Why, reg: u32, value: u32) -> Self {
        Refusal { why, reg: Some((reg, value)) }
    }
}

impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "gate {}: {}", self.why.gate(), self.why.text())?;
        if let Some((r, v)) = self.reg {
            write!(f, " ({r:#07x} = {v:#010x})")?;
        }
        Ok(())
    }
}
