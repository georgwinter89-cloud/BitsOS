//! The sink's audio description for the AMD display side of HDMI audio (SN-5): what Linux's amdgpu reads from
//! the monitor's EDID (`dm_helpers_parse_edid_caps`, `fill_audio_info`) and how `dce_aud_az_configure` turns it
//! into the AZ endpoint's registers. On AMD the codec's ELD is not written as bytes: the hardware answers the HDA
//! side's ELD verbs from these endpoint registers (SINK_INFO0-8, AUDIO_DESCRIPTOR0-13, CHANNEL_SPEAKER,
//! RESPONSE_LIPSYNC), so "the ELD from the EDID" is [`AzConfig::from_sink`] on [`Sink::from_edid`]. V1 wrote a
//! hard-coded iiyama sink (`snd/dpaudtab.asm` DPA_SINK0..DPA_NAME1); without an EDID V2 uses [`Sink::generic`].
//!
//! Linux references: `drivers/gpu/drm/drm_edid.c` (`get_monitor_name` 5443, `_drm_edid_to_sad` 5586,
//! `_drm_edid_to_speaker_allocation` 5639, `drm_parse_hdmi_vsdb_audio` 5405, `cea_db_collection_size` 4984,
//! `__cea_db_iter_edid_next` 4999), `amd/display/amdgpu_dm/amdgpu_dm_helpers.c:80` (`dm_helpers_parse_edid_caps`),
//! `amd/display/amdgpu_dm/amdgpu_dm.c:5570` (`fill_audio_info`), `amd/display/dc/dce/dce_audio.c:393`
//! (`dce_aud_az_configure`). Every length is checked; a hostile EDID gives an error or fewer facts, never a panic.

use alloc::vec::Vec;

/// One CEA short audio descriptor (`struct cea_sad`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sad {
    /// Audio format code (1 LPCM, 2 AC-3, ... 14 WMA Pro).
    pub format: u8,
    /// Channels - 1 (0..=7).
    pub channels: u8,
    /// Rate bits: 32, 44.1, 48, 88.2, 96, 176.4, 192 kHz from bit 0.
    pub freq: u8,
    /// LPCM: sample sizes (bit 0 16, bit 1 20, bit 2 24 bit); others: format specific.
    pub byte2: u8,
}

pub const FORMAT_LPCM: u8 = 1;
pub const FORMAT_AC3: u8 = 2;
/// The formats `dce_aud_az_configure` skips (`AUDIO_FORMAT_CODE_1BITAUDIO`, `_DST`): their descriptors stay 0.
pub const FORMAT_1BIT: u8 = 9;
pub const FORMAT_DST: u8 = 13;
/// `DEFAULT_SPEAKER_LOCATION` (`dc_types.h:160`): FL/FR and FC when the EDID has no speaker allocation block.
pub const DEFAULT_SPEAKERS: u8 = 5;
/// Monitor name characters kept (`get_monitor_name`: 13).
pub const NAME_MAX: usize = 13;

/// What amdgpu's `dc_edid_caps` holds of a sink for audio.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sink {
    /// EDID bytes 8-9 as amdgpu packs them (`mfg_id[0] | mfg_id[1] << 8`).
    pub manufacturer_id: u16,
    /// EDID bytes 10-11, little endian.
    pub product_id: u16,
    /// The monitor name descriptor's text (at most 13 bytes, no line feed).
    pub name: Vec<u8>,
    /// The first CEA audio data block's descriptors (empty without a CEA extension of revision 3 or later).
    pub sads: Vec<Sad>,
    /// Speaker allocation byte 0 (`DEFAULT_SPEAKERS` without the block).
    pub speakers: u8,
    /// HDMI VSDB progressive latencies (raw bytes: ms / 2 + 1), when the block says they are present.
    pub video_latency: Option<u8>,
    pub audio_latency: Option<u8>,
    /// The base block's checksum did not add up (Linux goes on with `EDID_BAD_CHECKSUM`, so do we).
    pub bad_checksum: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdidError {
    /// Fewer than 128 bytes.
    Short,
    /// Not the fixed EDID header 00 FF FF FF FF FF FF 00.
    Header,
}

const HEADER: [u8; 8] = [0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0];
const BLOCK: usize = 128;
const CEA_EXT: u8 = 0x02;
const TAG_AUDIO: u8 = 1;
const TAG_VENDOR: u8 = 3;
const TAG_SPEAKER: u8 = 4;
const HDMI_OUI: [u8; 3] = [0x03, 0x0C, 0x00];
const MONITOR_NAME: u8 = 0xFC;
const DTD: usize = 18;

impl Sink {
    /// A sink with no EDID to go by: LPCM stereo at 32/44.1/48 kHz, 16 bit, front left/right, no name (what V1's
    /// measured sink offered, without its iiyama identity).
    pub fn generic() -> Sink {
        Sink {
            manufacturer_id: 0,
            product_id: 0,
            name: Vec::new(),
            sads: alloc::vec![Sad { format: FORMAT_LPCM, channels: 1, freq: 0x07, byte2: 0x01 }],
            speakers: 1,
            video_latency: None,
            audio_latency: None,
            bad_checksum: false,
        }
    }

    /// Parses an EDID (base block and its extensions, as many as are there; a truncated extension is ignored).
    pub fn from_edid(edid: &[u8]) -> Result<Sink, EdidError> {
        let base = edid.get(..BLOCK).ok_or(EdidError::Short)?;
        if base[..8] != HEADER {
            return Err(EdidError::Header);
        }
        let bad_checksum = base.iter().fold(0u8, |a, &b| a.wrapping_add(b)) != 0;
        let count = base[126] as usize;
        let exts: Vec<&[u8]> = (1..=count).filter_map(|i| edid.get(i * BLOCK..(i + 1) * BLOCK)).collect();
        let mut s = Sink {
            manufacturer_id: base[8] as u16 | (base[9] as u16) << 8,
            product_id: base[10] as u16 | (base[11] as u16) << 8,
            name: monitor_name(base, &exts),
            sads: Vec::new(),
            speakers: DEFAULT_SPEAKERS,
            video_latency: None,
            audio_latency: None,
            bad_checksum,
        };
        // `fill_audio_info` takes the modes only with `cea_rev >= 3`, `cea_rev` being the first CEA extension's.
        let cea_rev = exts.iter().find(|e| e[0] == CEA_EXT).map_or(0, |e| e[1]);
        let mut audio_seen = false;
        let mut speaker_seen = false;
        for db in data_blocks(&exts) {
            let tag = db[0] >> 5;
            let payload = &db[1..];
            match tag {
                TAG_AUDIO if !audio_seen => {
                    audio_seen = true;
                    if cea_rev >= 3 {
                        s.sads = payload
                            .chunks_exact(3)
                            .map(|d| Sad { format: (d[0] & 0x78) >> 3, channels: d[0] & 7, freq: d[1] & 0x7F, byte2: d[2] })
                            .collect();
                    }
                }
                TAG_SPEAKER if !speaker_seen && payload.len() == 3 => {
                    speaker_seen = true;
                    s.speakers = payload[0];
                }
                TAG_VENDOR if payload.len() >= 5 && payload[..3] == HDMI_OUI => {
                    // `drm_parse_hdmi_vsdb_audio`: db[8] bit 7, db[9], db[10] with db[0] the tag byte.
                    if payload.len() >= 10 && db[8] & 0x80 != 0 {
                        s.video_latency = Some(db[9]);
                        s.audio_latency = Some(db[10]);
                    }
                }
                _ => {}
            }
        }
        // `dm_helpers_parse_edid_caps` returns before the speaker flags when there is no SAD: they stay 0.
        if s.sads.is_empty() {
            s.speakers = 0;
        }
        Ok(s)
    }

    /// True if the sink takes LPCM (the only format this kernel sends).
    pub fn has_lpcm(&self) -> bool {
        self.sads.iter().any(|m| m.format == FORMAT_LPCM && m.freq != 0)
    }
}

/// The CEA data blocks of every CEA extension of revision 3 or later (`__cea_db_iter_edid_next`), each with
/// its tag byte; a block running past the collection ends that collection.
fn data_blocks<'a>(exts: &[&'a [u8]]) -> Vec<&'a [u8]> {
    let mut out = Vec::new();
    for e in exts.iter().filter(|e| e[0] == CEA_EXT && e[1] >= 3) {
        let d = e[2] as usize;
        if !(4..=127).contains(&d) {
            continue;
        }
        let mut i = 4;
        while i < d {
            let len = (e[i] & 0x1F) as usize;
            if i + 1 + len > d {
                break;
            }
            out.push(&e[i..i + 1 + len]);
            i += 1 + len;
        }
    }
    out
}

/// `get_monitor_name`: the last monitor name descriptor among the base block's four and the CEA extensions'
/// detailed timings, up to 13 characters or the line feed.
fn monitor_name<'a>(base: &'a [u8], exts: &[&'a [u8]]) -> Vec<u8> {
    let mut found: Option<&'a [u8]> = None;
    let mut look = |b: &'a [u8]| {
        if b.len() == DTD && b[0] == 0 && b[1] == 0 && b[2] == 0 && b[3] == MONITOR_NAME {
            found = Some(&b[5..]);
        }
    };
    for k in 0..4 {
        look(&base[54 + k * DTD..54 + (k + 1) * DTD]);
    }
    for e in exts.iter().filter(|e| e[0] == CEA_EXT) {
        let d = e[2] as usize;
        if d < 4 {
            continue;
        }
        let mut i = d;
        while i + DTD <= 127 {
            look(&e[i..i + DTD]);
            i += DTD;
        }
    }
    found.map_or_else(Vec::new, |s| s.iter().take(NAME_MAX).take_while(|&&c| c != 0x0A).copied().collect())
}

/// The endpoint register values `dce_aud_az_configure` writes for a DP signal (SN-5 stage 3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AzConfig {
    /// CHANNEL_SPEAKER.SPEAKER_ALLOCATION (7 bits).
    pub speakers: u8,
    /// AUDIO_DESCRIPTOR0..13 (format code 1..14); the 1-bit audio and DST ones are never written.
    pub descriptors: [u32; 14],
    pub video_lipsync: u8,
    pub audio_lipsync: u8,
    /// SINK_INFO0..8.
    pub sink_info: [u32; 9],
}

impl AzConfig {
    pub fn from_sink(s: &Sink) -> AzConfig {
        let mut descriptors = [0u32; 14];
        for (i, d) in descriptors.iter_mut().enumerate() {
            let code = i as u8 + 1;
            if code == FORMAT_1BIT || code == FORMAT_DST {
                continue;
            }
            // `is_audio_format_supported`: of several entries the first with the most channels.
            let mut best: Option<&Sad> = None;
            for m in s.sads.iter().filter(|m| m.format == code) {
                if best.is_none_or(|b| m.channels > b.channels) {
                    best = Some(m);
                }
            }
            let Some(m) = best else { continue };
            // amdgpu never fills `max_bit_rate`/`vendor_specific`: byte 2 is the sample size for LPCM, else 0.
            let (byte2, stereo) = if code == FORMAT_LPCM { (m.byte2, m.freq) } else { (0, 0) };
            *d = (m.channels & 7) as u32 | (m.freq as u32) << 8 | (byte2 as u32) << 16 | (stereo as u32) << 24;
        }
        let mut sink_info = [0u32; 9];
        sink_info[0] = s.manufacturer_id as u32 | (s.product_id as u32) << 16;
        // The length counts the terminating zero, at most 18 (`MAX_HW_AUDIO_INFO_DISPLAY_NAME_SIZE_IN_CHARS`).
        sink_info[1] = (s.name.len() + 1).min(18) as u32;
        // SINK_INFO2/3 (port IDs) stay 0, as V1 and the measured firmware state (see notes, decision 4).
        for (k, &c) in s.name.iter().take(18).enumerate() {
            sink_info[4 + k / 4] |= (c as u32) << (8 * (k % 4));
        }
        // `set_video_latency` ignores values outside 0..=255 (a byte cannot be); absent latencies are 0.
        AzConfig {
            speakers: s.speakers & 0x7F,
            descriptors,
            video_lipsync: s.video_latency.unwrap_or(0),
            audio_lipsync: s.audio_latency.unwrap_or(0),
            sink_info,
        }
    }
}
