//! Synthetic codecs that answer verbs like the Mini-PC's two codecs are known to (feature
//! `synth`, for the fixture generator and tests; not for the kernel).
//!
//! Known facts (`BitsOS-Plattform/HARDWARE-MINIPC.md`): ALC269 10EC:0269 at C7:00.6 codec 0,
//! speaker pin 0x14 and headphone pin 0x15 both on DAC 0x02; AMD 1002:AA01 at C7:00.1 with HDMI
//! pins 3, 5, 7, 9, B, configuration 0x185600F0, pin 3 to converter 2, pin sense 0x7FFFFFFF
//! before DP audio is on. Everything else (other widgets, caps values, amp steps) follows public
//! ALC269 / AMD HDMI codec listings and is a guess until a real dump replaces the fixture.

use alloc::vec::Vec;

use crate::verb::*;
use crate::walk::dump_codec;
use crate::{Nid, NoResponse, VerbSource, dump::Dump};

#[derive(Debug, Clone, Default)]
pub struct SynthWidget {
    pub nid: Nid,
    pub caps: u32,
    pub pin_caps: u32,
    pub config: u32,
    pub out_amp: u32,
    pub in_amp: u32,
    pub pcm: u32,
    pub stream_formats: u32,
    pub conns: Vec<Nid>,
    /// Answer to F09.
    pub sense: u32,
    /// Answer to F7C (AMD pins).
    pub hbr: u32,
}

#[derive(Debug, Clone, Default)]
pub struct SynthCodec {
    pub vendor_id: u32,
    pub revision_id: u32,
    pub subsystem_id: u32,
    pub afg: Nid,
    pub afg_type: u32,
    pub afg_pcm: u32,
    pub afg_stream_formats: u32,
    pub afg_out_amp: u32,
    pub afg_in_amp: u32,
    /// Sorted by node, contiguous.
    pub widgets: Vec<SynthWidget>,
}

impl SynthCodec {
    fn widget(&self, nid: Nid) -> Option<&SynthWidget> {
        self.widgets.iter().find(|w| w.nid == nid)
    }

    fn widget_range(&self) -> u32 {
        match (self.widgets.first(), self.widgets.last()) {
            (Some(a), Some(b)) => (a.nid as u32) << 16 | (b.nid - a.nid + 1) as u32,
            _ => 0,
        }
    }
}

impl VerbSource for SynthCodec {
    fn get(&mut self, nid: Nid, verb: u32) -> Result<u32, NoResponse> {
        let param = |p: u8| verb == get_param(p);
        if nid == 0 {
            return Ok(match () {
                _ if param(PAR_VENDOR_ID) => self.vendor_id,
                _ if param(PAR_REVISION_ID) => self.revision_id,
                _ if param(PAR_NODE_COUNT) => (self.afg as u32) << 16 | 1,
                _ => 0,
            });
        }
        if nid == self.afg {
            return Ok(match () {
                _ if param(PAR_FG_TYPE) => self.afg_type,
                _ if param(PAR_PCM) => self.afg_pcm,
                _ if param(PAR_STREAM) => self.afg_stream_formats,
                _ if param(PAR_AMP_OUT_CAP) => self.afg_out_amp,
                _ if param(PAR_AMP_IN_CAP) => self.afg_in_amp,
                _ if param(PAR_NODE_COUNT) => self.widget_range(),
                _ if param(PAR_POWER_STATE) => 0x0000_000F, // D0-D3 supported
                _ if verb == GET_SUBSYSTEM_ID => self.subsystem_id,
                _ => 0,
            });
        }
        let w = self.widget(nid).ok_or(NoResponse)?;
        Ok(match () {
            _ if param(PAR_AUDIO_WIDGET_CAP) => w.caps,
            _ if param(PAR_PIN_CAP) => w.pin_caps,
            _ if param(PAR_AMP_OUT_CAP) => w.out_amp,
            _ if param(PAR_AMP_IN_CAP) => w.in_amp,
            _ if param(PAR_PCM) => w.pcm,
            _ if param(PAR_STREAM) => w.stream_formats,
            _ if param(PAR_CONNLIST_LEN) => w.conns.len() as u32,
            _ if verb == GET_CONFIG_DEFAULT => w.config,
            _ if verb == GET_PIN_SENSE => w.sense,
            _ if verb == ATI_GET_HBR_CONTROL => w.hbr,
            _ if verb & 0xFFF00 == 0xF0200 => {
                let start = (verb & 0xFF) as usize;
                w.conns
                    .iter()
                    .skip(start)
                    .take(4)
                    .enumerate()
                    .fold(0, |a, (i, &n)| a | (n as u32) << (8 * i))
            }
            _ => 0,
        })
    }
}

fn vendor(nid: Nid) -> SynthWidget {
    SynthWidget {
        nid,
        caps: 0x00F0_0000,
        ..SynthWidget::default()
    }
}

fn pin(nid: Nid, caps: u32, pin_caps: u32, config: u32, conns: &[Nid]) -> SynthWidget {
    let out_amp = if caps & 4 != 0 { 0x8000_0000 } else { 0 };
    let in_amp = if caps & 2 != 0 { 0x0027_0300 } else { 0 };
    SynthWidget {
        nid,
        caps,
        pin_caps,
        config,
        out_amp,
        in_amp,
        conns: conns.to_vec(),
        ..SynthWidget::default()
    }
}

fn mixer(nid: Nid, in_amp: u32, conns: &[Nid]) -> SynthWidget {
    SynthWidget {
        nid,
        caps: 0x0020_010B,
        in_amp,
        conns: conns.to_vec(),
        ..SynthWidget::default()
    }
}

/// ALC269 on the Mini-PC's analog HDA (C7:00.6), codec 0.
pub fn alc269() -> SynthCodec {
    const NC: u32 = 0x4111_11F0; // Realtek's "not connected"
    let dac = |nid| SynthWidget {
        nid,
        caps: 0x0000_041D,
        out_amp: 0x0002_5757,
        pcm: 0x000E_0560,
        stream_formats: 1,
        ..SynthWidget::default()
    };
    let adc = |nid, conn| SynthWidget {
        nid,
        caps: 0x0010_011B,
        in_amp: 0x8005_1F17,
        pcm: 0x000E_0560,
        stream_formats: 1,
        conns: [conn].to_vec(),
        ..SynthWidget::default()
    };
    let mut widgets = Vec::new();
    for nid in 0x02..=0x24 {
        widgets.push(match nid {
            0x02 | 0x03 => dac(nid),
            0x06 => SynthWidget {
                nid,
                caps: 0x0000_0211,
                pcm: 0x001E_07F0,
                stream_formats: 5,
                ..SynthWidget::default()
            },
            0x08 => adc(nid, 0x23),
            0x09 => adc(nid, 0x22),
            0x0B => mixer(nid, 0x8005_1F17, &[0x18, 0x19, 0x1A, 0x1B, 0x1D]),
            0x0C => mixer(nid, 0x8000_0000, &[0x02, 0x0B]),
            0x0D => mixer(nid, 0x8000_0000, &[0x03, 0x0B]),
            0x12 => pin(nid, 0x0040_0401, 0x0000_0020, NC, &[]),
            0x14 => pin(nid, 0x0040_058D, 0x0001_0014, 0x9017_0110, &[0x0C, 0x0D]),
            0x15 => pin(nid, 0x0040_058D, 0x0001_001C, 0x0221_101F, &[0x0C, 0x0D]),
            0x17 => pin(nid, 0x0040_050C, 0x0000_0010, NC, &[0x0C, 0x0D]),
            0x18 => pin(nid, 0x0040_058F, 0x0000_373E, 0x02A1_1030, &[0x0C, 0x0D]),
            0x19 => pin(nid, 0x0040_048B, 0x0000_3724, NC, &[]),
            0x1A | 0x1B => pin(nid, 0x0040_058F, 0x0000_373E, NC, &[0x0C, 0x0D]),
            0x1D => pin(nid, 0x0040_0400, 0x0000_0020, NC, &[]),
            0x1E => pin(nid, 0x0040_0781, 0x0000_0014, NC, &[0x06]),
            0x20 => SynthWidget {
                nid,
                caps: 0x00F0_0040,
                ..SynthWidget::default()
            },
            0x22 | 0x23 => mixer(
                nid,
                0x8000_0000,
                &[0x18, 0x19, 0x1A, 0x1B, 0x1D, 0x0B, 0x12],
            ),
            _ => vendor(nid),
        });
    }
    SynthCodec {
        vendor_id: 0x10EC_0269,
        revision_id: 0x0010_0100,
        subsystem_id: 0,
        afg: 0x01,
        afg_type: 0x0000_0101,
        afg_pcm: 0x000E_0560,
        afg_stream_formats: 1,
        afg_out_amp: 0,
        afg_in_amp: 0,
        widgets,
    }
}

/// AMD 1002:AA01 on the Radeon's HDMI/DP audio function (C7:00.1).
pub fn aa01() -> SynthCodec {
    let mut widgets = Vec::new();
    for nid in 0x02..=0x0B {
        widgets.push(if nid % 2 == 0 {
            SynthWidget {
                nid,
                caps: 0x0000_0221,
                ..SynthWidget::default()
            }
        } else {
            SynthWidget {
                nid,
                caps: 0x0040_0381,
                pin_caps: 0x0000_0094,
                config: 0x1856_00F0,
                conns: [nid - 1].to_vec(),
                sense: 0x7FFF_FFFF,
                hbr: 0x0000_0001,
                ..SynthWidget::default()
            }
        });
    }
    SynthCodec {
        vendor_id: 0x1002_AA01,
        revision_id: 0x0010_0700,
        subsystem_id: 0x00AA_0100,
        afg: 0x01,
        afg_type: 0x0000_0001,
        afg_pcm: 0x0002_0070,
        afg_stream_formats: 5,
        afg_out_amp: 0,
        afg_in_amp: 0,
        widgets,
    }
}

const NOTE: &str = "SYNTHETIC, not read from hardware. Made by `cargo run --example synth --features synth` \
     from the facts in BitsOS-Plattform/HARDWARE-MINIPC.md; widgets nobody measured follow public \
     codec listings. Replace with a real dump from the Mini-PC (DUMP-FORMAT.md).";

/// The fixture dump for `name` (`alc269` or `aa01`), header included.
pub fn fixture(name: &str) -> Option<Dump> {
    let (codec, pci, id) = match name {
        "alc269" => (alc269(), "C7:00.6 1022:15E3", "10EC:0269 (Realtek ALC269)"),
        "aa01" => (aa01(), "C7:00.1 1002:1640", "1002:AA01 (AMD HDMI/DP)"),
        _ => return None,
    };
    let header = [
        ("format", "hdacodec-dump 1"),
        ("synthetic", "yes"),
        ("note", NOTE),
        ("machine", "Mini-PC (AMD Ryzen 7 PRO 8845HS)"),
        ("pci", pci),
        ("codec", id),
    ];
    Some(dump_codec(codec, 0, &header))
}
