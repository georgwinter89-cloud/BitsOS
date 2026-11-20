//! One file decoded with symphonia (Georg's decision S2): probe by content (the extension as a hint), the
//! default audio track, packets to interleaved f32 with at most two channels (more are cut to the front pair).
//! A panic inside the decoder (a malformed file) is caught and becomes an error: input never ends the service.

use std::fs::File;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

pub struct Decoding {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track: u32,
    /// Seconds, when the container says.
    pub duration: Option<f64>,
    /// Packets that did not decode (skipped).
    pub bad_packets: u64,
    tmp: Vec<f32>,
}

/// A decoded piece: interleaved f32, `channels` 1 or 2.
pub struct Chunk {
    pub rate: u32,
    pub channels: u32,
    pub samples: Vec<f32>,
}

fn caught<T>(what: &str, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f))
        .unwrap_or_else(|_| Err(format!("{what}: the decoder failed on this file")))
}

impl Decoding {
    pub fn open(path: &Path) -> Result<Decoding, String> {
        caught("open", || {
            let f = File::open(path).map_err(|e| e.to_string())?;
            let mss = MediaSourceStream::new(Box::new(f), Default::default());
            let mut hint = Hint::new();
            if let Some(x) = path.extension().and_then(|x| x.to_str()) {
                hint.with_extension(&x.to_ascii_lowercase());
            }
            let format = symphonia::default::get_probe()
                .probe(
                    &hint,
                    mss,
                    FormatOptions::default(),
                    MetadataOptions::default(),
                )
                .map_err(|e| format!("format: {e}"))?;
            let t = format
                .default_track(TrackType::Audio)
                .ok_or("no audio track")?;
            let params = t
                .codec_params
                .as_ref()
                .and_then(|p| p.audio())
                .ok_or("no audio parameters")?;
            let decoder = symphonia::default::get_codecs()
                .make_audio_decoder(params, &AudioDecoderOptions::default())
                .map_err(|e| format!("codec: {e}"))?;
            let duration = match (t.num_frames, params.sample_rate) {
                (Some(n), Some(r)) if r > 0 => Some(n as f64 / r as f64),
                _ => None,
            };
            let track = t.id;
            Ok(Decoding {
                format,
                decoder,
                track,
                duration,
                bad_packets: 0,
                tmp: Vec::new(),
            })
        })
    }

    /// The next piece, None at the end of the track.
    pub fn next(&mut self) -> Result<Option<Chunk>, String> {
        caught("decode", || {
            loop {
                let p = match self.format.next_packet() {
                    Ok(Some(p)) => p,
                    Ok(None) => return Ok(None),
                    Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                        return Ok(None);
                    }
                    Err(Error::ResetRequired) => return Ok(None),
                    Err(e) => return Err(format!("read: {e}")),
                };
                if p.track_id != self.track {
                    continue;
                }
                let buf = match self.decoder.decode(&p) {
                    Ok(b) => b,
                    Err(Error::DecodeError(_)) => {
                        self.bad_packets += 1;
                        continue;
                    }
                    Err(e) => return Err(format!("decode: {e}")),
                };
                let rate = buf.spec().rate();
                let ch = buf.spec().channels().count().max(1);
                if buf.frames() == 0 {
                    continue;
                }
                self.tmp.clear();
                buf.copy_to_vec_interleaved(&mut self.tmp);
                let samples = if ch <= 2 {
                    std::mem::take(&mut self.tmp)
                } else {
                    self.tmp
                        .chunks_exact(ch)
                        .flat_map(|f| [f[0], f[1]])
                        .collect()
                };
                return Ok(Some(Chunk {
                    rate,
                    channels: ch.min(2) as u32,
                    samples,
                }));
            }
        })
    }
}
