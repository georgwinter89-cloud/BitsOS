//! SN-4: the ALSA kernel interface libasound talks to (card 0 = the mixer): `/dev/snd/pcmC0D0p` (`pcm`,
//! `xfer`, the refine engine in `params`), `/dev/snd/pcmC0D0c` (SN-9: `cpcm`, `cxfer`, recording),
//! `/dev/snd/controlC0` (`ctl`), `/proc/asound/{cards,devices,pcm,version}` and `card0/id`. ABI: `abi` (x86-64 layouts of `linux-6.6/include/uapi/sound/asound.h`).
//!
//! Card 0 always exists once the mixer runs: without a controller it plays into the mixer's null output (SN-3
//! decision 9), so programs that keep time by the sound clock (Cobalt) work on machines without sound. Every
//! open is its own mixer client; there is one subdevice that any number of programs open (mixed).

pub mod abi;
mod cpcm;
mod ctl;
pub(crate) mod cxfer;
pub mod params;
mod pcm;
mod procfs;
pub mod testkit;
pub mod umem;
pub(crate) mod xfer;

pub use cpcm::{CapFile, HW_CAPTURE};
pub use pcm::{constraints, Card, PcmFile, HW};
pub use xfer::f32_to_s16;

use alloc::format;
use alloc::sync::Arc;
use uapi::errno::*;
use vfs::node::File;
use vfs::Errno;

/// `CONFIG_SND_MAJOR`; minors as without dynamic minors (controlC0 0, pcmC0D0p 16, pcmC0D0c 24:
/// `include/sound/minors.h` SNDRV_MINOR_PCM_PLAYBACK/CAPTURE).
pub const ALSA_MAJOR: u32 = 116;
pub const CTL_MINOR: u32 = 0;
pub const PCM_MINOR: u32 = 16;
pub const PCM_CAPTURE_MINOR: u32 = 24;

pub const CARD_ID: &str = "BitsOS";
/// No `cards/<driver>.conf` exists for it, so alsa.conf's `default` is plug -> hw:0 (pcm/default.conf).
pub const DRIVER: &str = "BitsOS";
pub const CARD_NAME: &str = "BitsOS";
pub const LONGNAME: &str = "BitsOS Mixer (48 kHz, 16 bit)";
pub const MIXERNAME: &str = "BitsOS Mixer";
pub const PCM_ID: &str = "BitsOS PCM";
pub const PCM_NAME: &str = "BitsOS Mixer";

pub(crate) const PCM_NODE: &str = "/dev/snd/pcmC0D0p";
pub(crate) const PCM_CAPTURE_NODE: &str = "/dev/snd/pcmC0D0c";
pub(crate) const CTL_NODE: &str = "/dev/snd/controlC0";

/// B3: a program reaching the sound device (no audio class yet: `Other`, change request in the notes).
pub(crate) fn allowed(node: &'static str) -> Result<(), Errno> {
    allowed_class(node, policy::DeviceClass::Audio)
}

/// The microphone has its own class (SN-9: `DeviceClass::Microphone`).
pub(crate) fn allowed_class(node: &'static str, class: policy::DeviceClass) -> Result<(), Errno> {
    match proc::current() {
        Some(p) => policy::check(&p.subject(), &policy::Access::Device { class, name: node }).map_err(|_| EACCES),
        None => Ok(()),
    }
}

/// A PCM open: a new stream on the kernel mixer (started on first use).
fn open_pcm(flags: u64) -> Result<Arc<dyn File>, Errno> {
    allowed(PCM_NODE)?;
    let m = crate::mixer().map_err(|e| {
        klog::event!(Warn, "snd", "pcm.open_failed", err = format!("{e:?}").as_str());
        ENODEV
    })?;
    let name = match proc::current() {
        Some(p) => format!("pcm:{}", p.pid),
        None => "pcm:kernel".into(),
    };
    klog::event!(Debug, "snd", "pcm.open", name = name.as_str(), flags = flags);
    Ok(Arc::new(PcmFile::new(m, flags, name)))
}

/// `snd_pcm_info` of device 0's playback (`stream` 0: any number of programs, mixed) or capture substream
/// (1: one program at a time) (`struct snd_pcm_info`).
pub(crate) fn pcm_info(b: &mut [u8], stream: u32) {
    use abi::*;
    b.fill(0);
    put_u32(b, pcm_info::STREAM, stream);
    put_str(b, pcm_info::ID, 64, PCM_ID);
    put_str(b, pcm_info::NAME, 80, PCM_NAME);
    put_str(b, pcm_info::SUBNAME, 32, "subdevice #0");
    put_u32(b, pcm_info::COUNT, 1);
    put_u32(b, pcm_info::AVAIL, if stream == 1 && cpcm::opened() { 0 } else { 1 });
}

vfs::devnode!("/dev/snd/pcmC0D0p", pcm::node);
vfs::devnode!("/dev/snd/pcmC0D0c", cpcm::node);
vfs::devnode!("/dev/snd/controlC0", ctl::node);
