//! What played last, kept over a restart (the start card's "Musik · Zuletzt gehört" after a reboot, spec/
//! sofa-mode.md 2.1): one small file of `key=value` lines beside `playtime.txt`, written when a queue starts or
//! the output changes, read at start. Best effort: a missing or broken file is an empty memory, a failed write an
//! event; nothing here can end the service.

use std::path::{Path, PathBuf};

/// Bound of the file read (per input, R1).
pub const BYTES_MAX: u64 = 4096;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Last {
    /// `list`, `album`, `track` or `all` (empty: nothing played yet).
    pub kind: String,
    pub name: String,
    /// The last output a stream played to (`hdmi`, `jack`, `usb`, `none`, `null`).
    pub output: String,
}

pub fn default_path(uid: u32) -> PathBuf {
    PathBuf::from(format!(
        "/data/home/{uid}/.local/share/bitsos/bitsmusik-last.txt"
    ))
}

fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(crate::tags::TEXT_MAX)
        .collect::<String>()
        .trim()
        .to_string()
}

pub fn parse(text: &str) -> Last {
    let mut l = Last::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k.trim() {
            "kind" if matches!(v.trim(), "list" | "album" | "track" | "all") => {
                l.kind = v.trim().to_string()
            }
            "name" => l.name = clean(v),
            "output" => l.output = clean(v),
            _ => {}
        }
    }
    if l.kind.is_empty() || l.name.is_empty() {
        l.kind.clear();
        l.name.clear();
    }
    l
}

pub fn load(p: &Path) -> Last {
    use std::io::Read;
    let mut b = Vec::new();
    match std::fs::File::open(p) {
        Ok(f) => {
            let _ = f.take(BYTES_MAX).read_to_end(&mut b);
            parse(&String::from_utf8_lossy(&b))
        }
        Err(_) => Last::default(),
    }
}

pub fn text(l: &Last) -> String {
    format!(
        "kind={}\nname={}\noutput={}\n",
        clean(&l.kind),
        clean(&l.name),
        clean(&l.output)
    )
}

/// Writes through a temporary file and a rename (a cut write leaves the old memory).
pub fn save(p: &Path, l: &Last) -> Result<(), String> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, text(l)).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, p).map_err(|e| format!("{}: {e}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_garbage() {
        let l = Last {
            kind: "list".into(),
            name: "Abend\nmit Zeile".into(),
            output: "hdmi".into(),
        };
        let back = parse(&text(&l));
        assert_eq!(
            (back.kind.as_str(), back.name.as_str(), back.output.as_str()),
            ("list", "Abend mit Zeile", "hdmi")
        );
        assert_eq!(parse("kind=boese\nname=x\n"), Last::default());
        assert_eq!(parse("\u{0}\u{ff}==\n=\nname"), Last::default());
        assert_eq!(
            parse(&format!("kind=all\nname={}", "a".repeat(9000)))
                .name
                .len(),
            crate::tags::TEXT_MAX
        );
    }
}
