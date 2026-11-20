//! Tags of a music file from its first bytes (pure, bounded by the slice the scan read: [`HEAD_BYTES`]):
//! ID3v2.2/2.3/2.4 (MP3, also in front of a FLAC), FLAC's VORBIS_COMMENT block and the Vorbis comment packet of
//! an Ogg Vorbis stream. Anything cut or malformed gives the fields found so far, never a panic.

/// Bytes the scan reads per file for its tags (a cover picture in front of the text frames may push them out:
/// then the file name stands in).
pub const HEAD_BYTES: usize = 512 * 1024;
/// Characters kept per text field (a tag is input: a 10 MB "title" must not reach the socket).
pub const TEXT_MAX: usize = 512;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub track: Option<u32>,
}

impl Tags {
    fn merge(&mut self, o: Tags) {
        self.title = self.title.take().or(o.title);
        self.artist = self.artist.take().or(o.artist);
        self.album = self.album.take().or(o.album);
        self.track = self.track.or(o.track);
    }
}

/// The tags of a file whose first bytes are `head`.
pub fn read(head: &[u8]) -> Tags {
    let mut t = Tags::default();
    let mut rest = head;
    if rest.starts_with(b"ID3") {
        let (id3, len) = id3v2(rest);
        t.merge(id3);
        rest = rest.get(len..).unwrap_or(&[]);
    }
    if rest.starts_with(b"fLaC") {
        t.merge(flac(rest));
    } else if rest.starts_with(b"OggS") {
        t.merge(ogg(rest));
    }
    t
}

fn clean(s: &str) -> Option<String> {
    let s = s.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    let s: String = s
        .chars()
        .filter(|c| !c.is_control())
        .take(TEXT_MAX)
        .collect();
    (!s.is_empty()).then_some(s)
}

/// "3", "03/12" -> 3.
fn track_no(s: &str) -> Option<u32> {
    s.trim().split('/').next()?.trim().parse().ok()
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn syncsafe(b: &[u8]) -> usize {
    b[..4]
        .iter()
        .fold(0usize, |a, &x| (a << 7) | (x & 0x7f) as usize)
}

/// ID3v2 at the start of `b`: the tags and the tag's whole length (header, body, footer).
pub fn id3v2(b: &[u8]) -> (Tags, usize) {
    let mut t = Tags::default();
    if b.len() < 10 || &b[..3] != b"ID3" {
        return (t, 0);
    }
    let (major, flags) = (b[3], b[5]);
    let size = syncsafe(&b[6..10]);
    let total = 10
        + size
        + if major == 4 && flags & 0x10 != 0 {
            10
        } else {
            0
        };
    let end = (10 + size).min(b.len());
    let mut body: Vec<u8> = b[10..end].to_vec();
    // The whole tag unsynchronised (v2.2/2.3; v2.4 marks it per frame and the frames below read it the same).
    if flags & 0x80 != 0 {
        let mut out = Vec::with_capacity(body.len());
        let mut prev = 0u8;
        for &x in &body {
            if !(prev == 0xff && x == 0) {
                out.push(x);
            }
            prev = x;
        }
        body = out;
    }
    let mut at = 0usize;
    if flags & 0x40 != 0 && major >= 3 && body.len() >= 4 {
        // Extended header: v2.3 size excludes its own 4 bytes, v2.4's (syncsafe) includes them.
        at = if major == 3 {
            4 + be32(&body) as usize
        } else {
            syncsafe(&body)
        };
    }
    let (id_len, head_len) = if major == 2 { (3, 6) } else { (4, 10) };
    while at + head_len <= body.len() {
        let h = &body[at..at + head_len];
        if h[0] == 0 {
            break; // padding
        }
        let len = match major {
            2 => ((h[3] as usize) << 16) | ((h[4] as usize) << 8) | h[5] as usize,
            3 => be32(&h[4..8]) as usize,
            _ => syncsafe(&h[4..8]),
        };
        let start = at + head_len;
        let stop = start.saturating_add(len).min(body.len());
        let frame = &body[start..stop];
        let text = || id3_text(frame);
        match &h[..id_len] {
            b"TIT2" | b"TT2" => t.title = t.title.take().or_else(text),
            b"TPE1" | b"TP1" => t.artist = t.artist.take().or_else(text),
            b"TALB" | b"TAL" => t.album = t.album.take().or_else(text),
            b"TRCK" | b"TRK" => t.track = t.track.or_else(|| text().and_then(|s| track_no(&s))),
            _ => {}
        }
        at = start.saturating_add(len);
    }
    (t, total)
}

/// A text frame: encoding byte, then Latin-1, UTF-16 with BOM, UTF-16BE or UTF-8; the first of several values.
fn id3_text(f: &[u8]) -> Option<String> {
    let (&enc, s) = f.split_first()?;
    let text = match enc {
        0 => s.iter().map(|&c| c as char).collect::<String>(),
        1 | 2 => {
            let (be, s) = match s {
                [0xff, 0xfe, r @ ..] => (false, r),
                [0xfe, 0xff, r @ ..] => (true, r),
                r => (enc == 2, r),
            };
            let units: Vec<u16> = s
                .chunks_exact(2)
                .map(|c| {
                    if be {
                        u16::from_be_bytes([c[0], c[1]])
                    } else {
                        u16::from_le_bytes([c[0], c[1]])
                    }
                })
                .collect();
            String::from_utf16_lossy(&units)
        }
        3 => String::from_utf8_lossy(s).into_owned(),
        _ => return None,
    };
    clean(text.split('\0').next().unwrap_or(""))
}

/// A Vorbis comment block (vendor, count, "KEY=value" entries; little-endian lengths), as in FLAC and Ogg.
pub fn vorbis_comments(b: &[u8]) -> Tags {
    let mut t = Tags::default();
    let le = |at: usize| {
        b.get(at..at + 4)
            .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]) as usize)
    };
    let Some(vlen) = le(0) else { return t };
    let mut at = 4usize.saturating_add(vlen);
    let Some(count) = le(at) else { return t };
    at += 4;
    // The count is input: the bytes bound the loop, not the number it claims.
    for _ in 0..count {
        let Some(len) = le(at) else { break };
        let Some(e) = b.get(at + 4..(at + 4).saturating_add(len)) else {
            break;
        };
        at += 4 + len;
        let e = String::from_utf8_lossy(e);
        let Some((k, v)) = e.split_once('=') else {
            continue;
        };
        match k.to_ascii_uppercase().as_str() {
            "TITLE" => t.title = t.title.take().or_else(|| clean(v)),
            "ARTIST" => t.artist = t.artist.take().or_else(|| clean(v)),
            "ALBUM" => t.album = t.album.take().or_else(|| clean(v)),
            "TRACKNUMBER" => t.track = t.track.or_else(|| track_no(v)),
            _ => {}
        }
    }
    t
}

/// FLAC: the metadata blocks after "fLaC"; type 4 is VORBIS_COMMENT.
pub fn flac(b: &[u8]) -> Tags {
    let mut at = 4usize;
    while let Some(h) = b.get(at..at + 4) {
        let (last, ty) = (h[0] & 0x80 != 0, h[0] & 0x7f);
        let len = ((h[1] as usize) << 16) | ((h[2] as usize) << 8) | h[3] as usize;
        let start = at + 4;
        if ty == 4 {
            return vorbis_comments(b.get(start..(start + len).min(b.len())).unwrap_or(&[]));
        }
        if last || ty == 127 {
            break;
        }
        at = start + len;
    }
    Tags::default()
}

/// Ogg: the packets of the first logical stream; the Vorbis comment header is "\x03vorbis" + comments.
pub fn ogg(b: &[u8]) -> Tags {
    let mut at = 0usize;
    let mut serial = None;
    let mut packet: Vec<u8> = Vec::new();
    let mut packets = 0;
    while let Some(h) = b.get(at..at + 27) {
        if &h[..4] != b"OggS" {
            break;
        }
        let s = u32::from_le_bytes([h[14], h[15], h[16], h[17]]);
        let nseg = h[26] as usize;
        let Some(lacing) = b.get(at + 27..at + 27 + nseg) else {
            break;
        };
        let mut data = at + 27 + nseg;
        let mine = *serial.get_or_insert(s) == s;
        for &l in lacing {
            let Some(seg) = b.get(data..data + l as usize) else {
                return Tags::default();
            };
            data += l as usize;
            if !mine {
                continue;
            }
            packet.extend_from_slice(seg);
            if l < 255 {
                packets += 1;
                if packets == 2 {
                    return match packet.strip_prefix(b"\x03vorbis") {
                        Some(c) => vorbis_comments(c),
                        None => Tags::default(),
                    };
                }
                packet.clear();
            }
        }
        at = data;
    }
    Tags::default()
}
