//! Input streams (SN-9): the microphone and line inputs `hdacodec::input` finds, recorded through an input
//! descriptor (Linux `hda_controller.c` azx_pcm_* for SNDRV_PCM_STREAM_CAPTURE, `hdac_stream.c`). The slot
//! keeps the bytes the DMA wrote (`pos.played`, from LPIB) and the bytes read (`written`); a reader that falls a
//! whole ring behind lost data: the overrun is counted and reading goes on from the DMA position.

use super::*;
use hdacodec::input::{self, InputKind};

/// An input of this controller (`OutputId` = codec address and pin, as for outputs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputInfo {
    pub id: OutputId,
    pub kind: InputKind,
    /// Built in (fixed), not a jack.
    pub fixed: bool,
    pub converter: u8,
    pub formats: Vec<PcmFormat>,
    /// The pin can sense a plug (`input_plugged`).
    pub sense: bool,
}

impl<H: Hw> Controller<H> {
    /// Every input of every codec (none on the GPU controller, which is only read).
    pub fn inputs(&self) -> Vec<InputInfo> {
        if self.read_only() {
            return Vec::new();
        }
        let mut v = Vec::new();
        for c in self.codecs() {
            for i in &c.inputs {
                let caps = c.model.groups.iter().find(|g| g.nid == i.group).and_then(|g| g.widget(i.converter())).map_or(0, |w| w.pcm.0);
                v.push(InputInfo {
                    id: OutputId { cad: c.cad, pin: i.pin },
                    kind: i.kind,
                    fixed: i.fixed,
                    converter: i.converter(),
                    formats: fmtw::formats(caps),
                    sense: i.jack.is_some(),
                });
            }
        }
        v
    }

    /// The plug state of an input's jack, read now (verbs: thread context). None without presence detect.
    pub fn input_plugged(&self, id: OutputId) -> Option<bool> {
        if self.state() != State::Up || self.read_only() {
            return None;
        }
        let c = self.codecs().into_iter().find(|c| c.cad == id.cad)?;
        let j = c.inputs.iter().find(|i| i.pin == id.pin)?.jack?;
        let mut g = self.link.lock_timeout(2_000_000)?;
        let l = g.as_mut()?;
        self.sense(l, c.cad, &j, false)
    }

    /// Opens a capture stream on input `id` (not running yet): an input descriptor, its ring and BDL, the path
    /// set up by `hdacodec::input::input_setup`.
    pub fn open_input(&self, id: OutputId, fmt: PcmFormat, ring_frames: u32, period_frames: u32) -> Result<StreamId, HdaError> {
        if self.state() != State::Up || self.read_only() {
            return Err(HdaError::NotUp);
        }
        let info = self.inputs().into_iter().find(|i| i.id == id).ok_or(HdaError::NoSuchOutput)?;
        let word = fmtw::format_word(fmt).ok_or(HdaError::BadFormat)?;
        if !info.formats.contains(&fmt) {
            return Err(HdaError::BadFormat);
        }
        let layout = Layout::new(fmtw::frame_bytes(fmt), ring_frames, period_frames).ok_or(HdaError::BadFormat)?;
        let codec = self.codecs().into_iter().find(|c| c.cad == id.cad).ok_or(HdaError::NoSuchOutput)?;
        let inp = codec.inputs.iter().find(|i| i.pin == id.pin).ok_or(HdaError::NoSuchOutput)?.clone();
        let mut slots = self.slots.lock();
        if slots.iter().flatten().any(|s| s.out.cad == id.cad && s.converter == info.converter) {
            return Err(HdaError::Busy);
        }
        let held: u64 = slots.iter().flatten().map(|s| s.layout.ring_bytes as u64).sum();
        if layout.ring_bytes > stream::MAX_RING_BYTES || held + layout.ring_bytes as u64 > stream::MAX_RING_BYTES as u64 {
            klog::event!(Warn, "snd", "hda.open_failed", ctrl = self.name.as_str(), what = "ring_size", bytes = layout.ring_bytes,
                held = held, limit = stream::MAX_RING_BYTES);
            return Err(HdaError::NoMemory);
        }
        let nout = self.nout();
        let k = slots.get(nout..).and_then(|v| v.iter().position(Option::is_none)).ok_or(HdaError::Busy)?;
        let n = nout + k;
        let desc = self.desc_of(n);
        let tag = k as u8 + 1;
        let fail = |what: &str, e: &str| {
            klog::event!(Warn, "snd", "hda.open_failed", ctrl = self.name.as_str(), what = what, error = e);
            HdaError::NoMemory
        };
        let bdl = self.alloc(layout.bdl_bytes(), DmaUse::Control).map_err(|e| fail("bdl", &e))?;
        let data = self.alloc(layout.ring_bytes as usize, DmaUse::Data).map_err(|e| fail("ring", &e))?;
        let ioc = self.irq_driven();
        if !stream::write_bdl(&bdl, data.dev_addr(), &layout, ioc) {
            return Err(fail("bdl", "write"));
        }
        let st = setup::Stream { tag, channel: 0, format: word };
        let actions = input::input_setup(&codec.model, &inp, &st).map_err(|e| {
            klog::event!(Warn, "snd", "hda.setup_refused", ctrl = self.name.as_str(), pin = klog::Hex(id.pin as u64), error = alloc::format!("{e:?}"));
            HdaError::Codec
        })?;
        if let Err(e) = self.run_actions(id.cad, &actions) {
            let _ = self.clear_converter(id.cad, info.converter);
            return Err(e);
        }
        if !stream::reset(&self.hw, desc) {
            klog::event!(Warn, "snd", "hda.stream_reset_slow", ctrl = self.name.as_str(), desc = desc);
        }
        stream::program(&self.hw, desc, tag, word, bdl.dev_addr(), &layout, !self.snooped());
        let g = self.generation.fetch_add(1, Ordering::AcqRel).wrapping_add(1) & 0xFF_FFFF;
        let sid = g << 8 | n as u32;
        klog::event!(Info, "snd", "hda.open_input", ctrl = self.name.as_str(), stream = sid, desc = desc, tag = tag, cad = id.cad,
            pin = klog::Hex(id.pin as u64), adc = klog::Hex(info.converter as u64), rate = fmt.rate, bits = fmt.bits,
            ring_frames = layout.ring_frames(), period_frames = layout.period_frames(), ioc = ioc, snoop = self.snooped());
        slots[n] = Some(Slot {
            id: sid,
            input: true,
            overruns: 0,
            out: id,
            converter: info.converter,
            fmt,
            layout,
            _bdl: bdl,
            data,
            pos: Position::default(),
            written: 0,
            running: false,
            periods_seen: 0,
            callback: None,
            underruns: 0,
            errors: 0,
        });
        Ok(sid)
    }

    /// Copies whole recorded frames out of the ring (0 when nothing is there). Never waits.
    pub fn read(&self, sid: StreamId, out: &mut [u8]) -> Result<usize, HdaError> {
        self.with_slot(sid, false, |s, desc| {
            if !s.input {
                return Err(HdaError::NoSuchStream);
            }
            let avail = Self::input_avail(&self.name, &self.hw, s, desc);
            let fb = s.layout.frame_bytes as u64;
            let ring = s.layout.ring_bytes as u64;
            let n = (out.len() as u64).min(avail) / fb * fb;
            let mut done = 0u64;
            while done < n {
                let at = (s.written + done) % ring;
                let chunk = (ring - at).min(n - done);
                if !s.data.copy_out(at as usize, &mut out[done as usize..(done + chunk) as usize]) {
                    return Err(HdaError::NoSuchStream);
                }
                done += chunk;
            }
            s.written += n;
            Ok(n as usize)
        })?
    }

    /// Recorded frames waiting to be read.
    pub fn readable(&self, sid: StreamId) -> Result<u32, HdaError> {
        self.with_slot(sid, false, |s, desc| {
            let a = if s.input { Self::input_avail(&self.name, &self.hw, s, desc) } else { 0 };
            (a / s.layout.frame_bytes as u64).min(u32::MAX as u64) as u32
        })
    }

    /// Overruns of an input stream (the reader fell a ring behind).
    pub fn overruns(&self, sid: StreamId) -> Result<u64, HdaError> {
        self.with_slot(sid, false, |s, _| s.overruns)
    }

    /// Bytes readable; a reader more than a ring minus one period behind (the DMA may be writing that period)
    /// lost data: counted, and it goes on from the DMA position.
    fn input_avail(name: &str, hw: &H, s: &mut Slot<H::Mem>, desc: usize) -> u64 {
        if s.running {
            Self::update_pos(hw, s, desc);
        }
        let lag = s.pos.played.saturating_sub(s.written);
        if lag > (s.layout.ring_bytes - s.layout.period_bytes) as u64 {
            s.overruns += 1;
            s.written = s.pos.played;
            if s.overruns == 1 || s.overruns.is_power_of_two() {
                klog::event!(Warn, "snd", "hda.overrun", ctrl = name, stream = s.id, count = s.overruns, lost_bytes = lag);
            }
            return 0;
        }
        lag
    }
}
