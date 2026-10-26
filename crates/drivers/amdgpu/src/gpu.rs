//! GX-D1: one device object for the whole GPU function (C7:00.0 on the Mini-PC): PCI identity and BARs, the MMIO
//! owner (`dcn::DcnRegs`, BAR5), the IP blocks by version (GX-D2), the display engine's attach (V1 gates 1-8,
//! read-only) and the buffer-object manager over the carve-out (GX-D3). Later the IH ring (GX-D6), PSP, SMU
//! and GFX hang off this object, not off the display.
//!
//! Nothing here writes a register or PCI configuration. `get()` attaches on first use (thread context: it may
//! map BAR5 and logs); QEMU has no AMD display function, so it is `None` there.

use crate::attach::{self, Attach, BridgeWindows, Gop, PciFacts, Refusal};
use crate::bo::{self, BoManager};
use crate::dcn::DcnRegs;
use crate::ip::{self, IpBlock};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ksync::{Once, SpinLock};

pub struct Gpu {
    pub regs: &'static DcnRegs,
    pub device: u16,
    pub revision: u8,
    /// BAR0 (the graphics memory window): phys, size.
    pub bar0: Option<(u64, u64)>,
    pub blocks: Vec<IpBlock>,
    /// The display engine as found at first use.
    pub display: Result<Attach, Refusal>,
    /// The carve-out's buffer objects (only with an attached display: its registers give the carve-out).
    pub bos: Option<SpinLock<BoManager>>,
}

static GPU: Once<Option<Gpu>> = Once::new();

impl Gpu {
    /// The device object, built on first use (thread context).
    pub fn get() -> Option<&'static Gpu> {
        GPU.get_or_init(build).as_ref()
    }

    /// Already built (never builds): for contexts that must not map or log.
    pub fn get_now() -> Option<&'static Gpu> {
        GPU.get().and_then(Option::as_ref)
    }

    pub fn function(&self) -> &str {
        self.regs.function()
    }

    /// The gates evaluated now (reads only; for the snapshot tool and a later re-attach after a mode change).
    pub fn check_display(&self) -> Result<Attach, Refusal> {
        evaluate(self.regs)
    }
}

/// The GOP framebuffer from the BootInfo: stride × height × 4 bytes (V1 `dcn_image`); none when headless.
pub fn gop() -> Option<Gop> {
    let (bi, _) = kexec::boot::boot_info()?;
    let fb = bi.framebuffer().filter(|f| f.format != bootinfo::PixelFormat::None && f.phys != 0)?;
    let bytes = u64::from(fb.stride) * u64::from(fb.height) * 4;
    (bytes != 0).then_some(Gop { phys: fb.phys, bytes })
}

/// Gates 1-2's facts for the function `DcnRegs` attached: configuration reads and the scan's bridge windows.
pub fn pci_facts(regs: &DcnRegs) -> PciFacts {
    let f = regs.pci();
    let bar0 = match f.bar(0) {
        Some(pci::Bar::Mem { phys, .. }) => Some(phys),
        _ => None,
    };
    let bar5 = match f.bar(5) {
        Some(pci::Bar::Mem { phys, size, is64: false, .. }) => Some((phys, size)),
        _ => None,
    };
    let nodes = pci::tree();
    let bridges = nodes.iter().position(|n| n.loc == f.loc()).and_then(|i| {
        let up = pci::node::ancestors(&nodes, i);
        let mut v = Vec::new();
        for p in up {
            let b = nodes[p].bridge.as_ref()?;
            v.push(BridgeWindows {
                mem: b.mem.map(|w| (w.start, w.end)),
                prefetch: b.prefetch.map(|w| (w.start, w.end)),
            });
        }
        Some(v)
    });
    PciFacts {
        function: format!("{}", f.addr),
        bus: f.addr.bus,
        vendor: f.vendor,
        class_code: f.class_code(),
        command: f.command(),
        bar0,
        bar5,
        bridges,
    }
}

/// Every gate on the live registers (reads only).
pub fn evaluate(regs: &DcnRegs) -> Result<Attach, Refusal> {
    attach::gates(&pci_facts(regs), gop(), regs)
}

fn build() -> Option<Gpu> {
    let regs = DcnRegs::attached()?;
    let f = regs.pci();
    let bar0 = match f.bar(0) {
        Some(b @ pci::Bar::Mem { .. }) => Some((b.start(), b.size())),
        _ => None,
    };
    let mut blocks = ip::known_blocks(f.device);
    let display = evaluate(regs);
    match &display {
        Ok(a) => {
            ip::mark_dcn_measured(&mut blocks);
            klog::event!(Info, "dcn", "display.attached", function = regs.function(), a0 = a.a0, a1 = a.a1, bytes = a.bytes,
                pipes = a.pipes.len(), dmcub = a.dmcub_running, windows = a.windows.len(), settle = format!("{:?}", a.settle).as_str();
                "display engine attached read-only (V1 gates 1-8)");
        }
        Err(e) => {
            let why = format!("{e}");
            klog::event!(Warn, "dcn", "display.refused", function = regs.function(), gate = e.why.gate(), why = why.as_str();
                "display engine not used: one image");
        }
    }
    let bos = display.as_ref().ok().map(|a| {
        SpinLock::named("amdgpu.bos", BoManager::new((a.fb_last - a.fb_first).saturating_add(1), bo::reserved_from(a)))
    });
    Some(Gpu { regs, device: f.device, revision: f.revision, bar0, blocks, display, bos })
}

/// A short text of the device object (the snapshot tool's first lines).
pub fn describe(g: &Gpu) -> Vec<String> {
    let mut v = alloc::vec![format!(
        "gpu {} 1002:{:04X} rev {:#04x}, BAR0 {}, BAR5 {:#x}",
        g.function(),
        g.device,
        g.revision,
        g.bar0.map_or(String::from("none"), |(p, s)| format!("{p:#x} ({} MB)", s >> 20)),
        g.regs.bar5().0
    )];
    for b in &g.blocks {
        let src = match b.source {
            ip::Source::Table(_) => "table",
            ip::Source::Measured(_) => "measured",
            ip::Source::Discovery => "discovery",
        };
        v.push(format!("ip {:?}{} {} ({src})", b.kind, b.instance, b.version));
    }
    v
}
