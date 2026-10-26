//! GX-D5: the display path in KMS terms, read from the registers (read-only). CRTC = OTG (timing generator),
//! plane = HUBP (+ its DPP), encoder = DIG (front end + back end), connector = the DP link behind a DIG.
//! Framebuffer = a buffer object (`bo`). A DRM/KMS uAPI and bitscomp map onto these 1:1 later (V2-PLAN §8a.7);
//! the atomic commit with in/out fences and vblank events (sequence, timestamp) is GX-3's (design note).
//!
//! Linux names: `dc/dcn10/dcn10_link_encoder.c:884-910` (DIG_MODE values), `dcn20_hubp.c` (surface registers),
//! `dcn10_hw_sequencer.c:1333` (every OTG's OTG_CONTROL is read at init, so reading all four is what Linux does).

use crate::attach::{self, Regs};
use crate::layout as l;
use alloc::vec::Vec;
use dcn314_regs::{self as r, Field};

fn fld<R>(f: Field<R>, raw: u32) -> u32 {
    (raw & f.mask()) >> f.shift()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crtc {
    /// OTG index.
    pub id: u8,
    pub enabled: bool,
    pub h_total: u32,
    pub v_total: u32,
    pub h_active: u32,
    pub v_active: u32,
    /// Pixel clock from the DP DTO (PHASE in Hz when MODULO is the 600 MHz dprefclk); `None` without the DTO.
    pub pixel_hz: Option<u64>,
    /// Millihertz, from pixel clock and totals.
    pub refresh_mhz: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plane {
    /// HUBP index.
    pub id: u8,
    /// The OTG that times it (HUBP_VTG_SEL).
    pub crtc: u8,
    pub visible: bool,
    pub address: u64,
    pub width: u32,
    pub height: u32,
    /// Pitch in pixels.
    pub pitch: u32,
    /// SURFACE_PIXEL_FORMAT (8 = ARGB8888).
    pub format: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderMode {
    DpSst,
    Lvds,
    Dvi,
    Hdmi,
    DpMst,
    Other(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Encoder {
    /// DIG index.
    pub id: u8,
    /// The OTG its front end takes pixels from.
    pub crtc: Option<u8>,
    pub mode: EncoderMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectorKind {
    /// DP SST: on the Mini-PC the board's DP-to-HDMI converter (spec/gpu.md §2.2 "Link").
    DisplayPort,
    Hdmi,
    Dvi,
    Lvds,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Connector {
    pub id: u8,
    pub encoder: u8,
    pub kind: ConnectorKind,
    /// DP only: DP_LINK_TRAINING_COMPLETE and DP_VID_STREAM_ENABLE.
    pub link_trained: bool,
    pub stream_on: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Topology {
    pub crtcs: Vec<Crtc>,
    pub planes: Vec<Plane>,
    pub encoders: Vec<Encoder>,
    pub connectors: Vec<Connector>,
}

/// Reads the topology; an unreadable register leaves its object out (never a panic on device input). Narrow
/// (`wide` false): only OTG0, an OTG a powered pipe drives, and DIG0/DP0, as `snapshot::take`.
pub fn read(regs: &impl Regs, wide: bool) -> Topology {
    let mut t = Topology::default();
    let driven = if wide { u32::MAX } else { attach::driven_otgs(regs) | 1 };
    for k in (0..l::OTGS).filter(|k| driven & (1 << k) != 0) {
        let Some(c) = regs.read(l::otg(k, l::OTG_CONTROL)) else {
            continue;
        };
        let rd = |b| regs.read(l::otg(k, b)).unwrap_or(0);
        let (ht, vt) = (
            fld(r::otg0_otg_h_total::OTG_H_TOTAL, rd(l::OTG_H_TOTAL)) + 1,
            fld(r::otg0_otg_v_total::OTG_V_TOTAL, rd(l::OTG_V_TOTAL)) + 1,
        );
        let hb = rd(l::OTG_H_BLANK);
        let vb = rd(l::OTG_V_BLANK);
        let h_active = fld(r::otg0_otg_h_blank_start_end::OTG_H_BLANK_START, hb)
            .saturating_sub(fld(r::otg0_otg_h_blank_start_end::OTG_H_BLANK_END, hb));
        let v_active = fld(r::otg0_otg_v_blank_start_end::OTG_V_BLANK_START, vb)
            .saturating_sub(fld(r::otg0_otg_v_blank_start_end::OTG_V_BLANK_END, vb));
        let dto = regs.read(l::dto_cntl(k)).unwrap_or(0);
        let phase = regs.read(l::dto_phase(k)).unwrap_or(0);
        let modulo = regs.read(l::dto_modulo(k)).unwrap_or(0);
        let pixel_hz =
            (dto & l::DTO_ON == l::DTO_ON && phase != 0 && modulo == l::DTO_MODULO).then_some(u64::from(phase));
        let refresh_mhz = pixel_hz.and_then(|p| u32::try_from(p * 1000 / (u64::from(ht) * u64::from(vt))).ok());
        t.crtcs.push(Crtc {
            id: k as u8,
            enabled: c & l::OTG_MASTER_EN != 0,
            h_total: ht,
            v_total: vt,
            h_active,
            v_active,
            pixel_hz,
            refresh_mhz,
        });
    }
    for p in 0..l::PIPES {
        if !attach::powered(regs, p).unwrap_or(false) {
            continue;
        }
        let rd = |b| regs.read(l::hubp(p, b));
        let (Some(cntl), Ok(address)) = (rd(l::HUBP_CNTL), attach::surface(regs, p)) else {
            continue;
        };
        let vp = rd(l::HUBP_VIEWPORT).unwrap_or(0);
        t.planes.push(Plane {
            id: p as u8,
            crtc: fld(r::hubp0_dchubp_cntl::HUBP_VTG_SEL, cntl) as u8,
            visible: cntl & l::BLANKED == 0,
            address,
            width: fld(r::hubp0_dcsurf_pri_viewport_dimension::PRI_VIEWPORT_WIDTH, vp),
            height: fld(r::hubp0_dcsurf_pri_viewport_dimension::PRI_VIEWPORT_HEIGHT, vp),
            pitch: fld(r::hubpreq0_dcsurf_surface_pitch::PITCH, rd(l::HUBP_PITCH).unwrap_or(0)) + 1,
            format: fld(r::hubp0_dcsurf_surface_config::SURFACE_PIXEL_FORMAT, rd(l::HUBP_SURFACE_CONFIG).unwrap_or(0)),
        });
    }
    for d in 0..if wide { l::DIGS } else { 1 } {
        let Some(en) = regs.read(l::dig(d, l::DIG_BE_EN_CNTL)) else {
            continue;
        };
        if fld(r::dig0_dig_be_en_cntl::DIG_ENABLE, en) == 0 {
            continue;
        }
        let be = regs.read(l::dig(d, l::DIG_BE_CNTL)).unwrap_or(0);
        let mode = match fld(r::dig0_dig_be_cntl::DIG_MODE, be) {
            0 => EncoderMode::DpSst,
            1 => EncoderMode::Lvds,
            2 => EncoderMode::Dvi,
            3 => EncoderMode::Hdmi,
            5 => EncoderMode::DpMst,
            m => EncoderMode::Other(m as u8),
        };
        // The back end takes the front end named by its one-hot FE_SOURCE_SELECT; that front end's source is an OTG.
        let fe_mask = fld(r::dig0_dig_be_cntl::DIG_FE_SOURCE_SELECT, be);
        let crtc = (fe_mask != 0)
            .then(|| fe_mask.trailing_zeros())
            .filter(|&fe| fe < l::DIGS && (wide || fe == 0))
            .and_then(|fe| regs.read(l::dig(fe, l::DIG_FE_CNTL)))
            .map(|fe| fld(r::dig0_dig_fe_cntl::DIG_SOURCE_SELECT, fe) as u8)
            .filter(|&k| u32::from(k) < l::OTGS);
        t.encoders.push(Encoder { id: d as u8, crtc, mode });
        let kind = match mode {
            EncoderMode::DpSst | EncoderMode::DpMst => ConnectorKind::DisplayPort,
            EncoderMode::Hdmi => ConnectorKind::Hdmi,
            EncoderMode::Dvi => ConnectorKind::Dvi,
            EncoderMode::Lvds => ConnectorKind::Lvds,
            EncoderMode::Other(_) => ConnectorKind::Other,
        };
        let dp = kind == ConnectorKind::DisplayPort;
        let link = if dp { regs.read(l::dp(d, l::DP_LINK_CNTL)).unwrap_or(0) } else { 0 };
        let stream = if dp { regs.read(l::dp(d, l::DP_VID_STREAM_CNTL)).unwrap_or(0) } else { 0 };
        t.connectors.push(Connector {
            id: d as u8,
            encoder: d as u8,
            kind,
            link_trained: fld(r::dp0_dp_link_cntl::DP_LINK_TRAINING_COMPLETE, link) != 0,
            stream_on: fld(r::dp0_dp_vid_stream_cntl::DP_VID_STREAM_ENABLE, stream) != 0,
        });
    }
    t
}

impl Topology {
    /// One line per object, for the snapshot tool's text answer and the log.
    pub fn lines(&self) -> Vec<alloc::string::String> {
        use alloc::format;
        let mut v = Vec::new();
        for c in self.crtcs.iter().filter(|c| c.enabled) {
            v.push(format!(
                "crtc{} (OTG{}): {}x{} of {}x{}, pixel clock {} Hz, {}.{:03} Hz",
                c.id,
                c.id,
                c.h_active,
                c.v_active,
                c.h_total,
                c.v_total,
                c.pixel_hz.unwrap_or(0),
                c.refresh_mhz.unwrap_or(0) / 1000,
                c.refresh_mhz.unwrap_or(0) % 1000
            ));
        }
        for p in self.planes.iter().filter(|p| p.visible) {
            v.push(format!(
                "plane{} (HUBP{}) on crtc{}: {}x{} pitch {} format {} at GPU {:#x}",
                p.id, p.id, p.crtc, p.width, p.height, p.pitch, p.format, p.address
            ));
        }
        for e in &self.encoders {
            v.push(format!("encoder{} (DIG{}) {:?} from crtc{}", e.id, e.id, e.mode, e.crtc.map_or(-1, i32::from)));
        }
        for c in &self.connectors {
            v.push(format!(
                "connector{} {:?} on encoder{}: link trained {}, stream {}",
                c.id, c.kind, c.encoder, c.link_trained, c.stream_on
            ));
        }
        v
    }
}
