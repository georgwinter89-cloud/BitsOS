//! EINS-D: the clipboard and pictures for `bits` (`bits-clip-get`, `bits-clip-set`, `bits-foto`). The clipboard's
//! owner is asked as a paste asks it (`clip_read`, on a thread with its deadlines: B4); bitscomp itself becomes the
//! owner of a text it is given, as for "Bericht für Claude kopieren" (`sys_clip`). Files are taken and written only
//! in the user's runtime folder, by the names bits gives them, never through a link.

use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use smithay::wayland::selection::data_device::set_data_device_selection;

use crate::clip_read::{self, Limits, Source, Why};
use crate::ctl_bits::reply;
use crate::ctl_bits_gate::{NEVER_SEE, never_seen};
use crate::render::Composer;
use crate::state::App;
use crate::ui_reg::Full;

/// The most text taken or given at once (bits says the same).
pub const MAX: usize = 1 << 20;

fn json_text(t: &str) -> String {
    serde_json::json!({ "text": t }).to_string()
}

fn why(w: Why) -> &'static str {
    match w {
        Why::Empty => "Die Zwischenablage ist leer (oder hält keinen Text).",
        Why::Slow => "Das Programm, dem die Zwischenablage gehört, gibt ihren Text nicht heraus.",
        Why::TooLong => "Der Text in der Zwischenablage ist zu lang.",
        Why::Failed => "Die Zwischenablage ließ sich nicht lesen.",
    }
}

/// From the tree the clipboard is not read while a program on `assistant.never_see` has a window open (on any
/// workspace, minimised too): bitscomp does not know which program copied the text, and a password copied from such
/// a program must not reach the assistant (never_see, F1 "never echoed"). The same as a whole-screen `bits-foto`.
pub fn hidden_from_tree(app: &App) -> bool {
    app.all_windows().iter().any(|w| never_seen(app, &crate::ctl::names(w).0))
}

pub const CLIP_NEVER_SEE: &str = "Ein Programm von der Liste „bei diesen Programmen nie“ ist offen; solange liest der Assistent die Zwischenablage nicht.";

pub fn get(app: &mut App, stream: UnixStream, tree: bool) {
    if tree && hidden_from_tree(app) {
        reply(stream, &format!("refused: {CLIP_NEVER_SEE}"));
        return;
    }
    match app.clip_source() {
        Source::Own(t) => reply(stream, &json_text(&t)),
        Source::None => reply(stream, &format!("error: {}", why(Why::Empty))),
        Source::Pipe(r) => {
            let lim = Limits { cap: MAX, idle: clip_read::IDLE, total: clip_read::TOTAL, line: false };
            let spawned = std::thread::Builder::new().name("bitscomp-bits-clip".into()).spawn(move || {
                let a = match clip_read::read(r, lim) {
                    Ok(t) => json_text(&t),
                    Err(w) => format!("error: {}", why(w)),
                };
                reply(stream, &a);
            });
            if let Err(e) = spawned {
                eprintln!("bitscomp: bits: no thread to read the clipboard: {e}");
            }
        }
    }
}

/// A path bits named: directly in the runtime folder, with bits's prefix.
fn own_file(words: &[&str], prefix: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(words.get(1).ok_or("Der Pfad fehlt.")?);
    let named = p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(prefix) && !n.contains('/'));
    if !named || p.parent() != Some(crate::rundir::dir().as_path()) {
        return Err("Diese Datei nimmt die Oberfläche nicht.".into());
    }
    Ok(p)
}

pub fn set(app: &mut App, words: &[&str], uid: u32) -> Result<String, String> {
    let p = own_file(words, ".bits-clip-")?;
    let m = std::fs::symlink_metadata(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    if !m.is_file() || m.uid() != uid || m.len() > MAX as u64 {
        return Err("Diese Datei nimmt die Oberfläche nicht.".into());
    }
    let mut f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(&p).map_err(|e| e.to_string())?;
    let mut text = String::new();
    Read::by_ref(&mut f).take(MAX as u64).read_to_string(&mut text).map_err(|_| "Das ist kein Text.".to_string())?;
    eprintln!("bitscomp: bits: the clipboard holds {} bytes from bits now", text.len());
    app.sys.clip = Some(text);
    set_data_device_selection(&app.dh, &app.seat, crate::sys_clip::MIMES.iter().map(|m| m.to_string()).collect(), ());
    Ok("ok".into())
}

/// `bits-foto <path> [window]`: the screen (or the window's rectangle on it) as PPM.
pub fn foto(app: &mut App, comp: &mut dyn Composer, words: &[&str], tree: bool) -> Result<String, String> {
    let p = own_file(words, ".bits-foto-")?;
    let windows = app.ui_windows(&Full::Local(String::new()));
    let rect = match words.get(2) {
        Some(w) => {
            let x = windows.iter().find(|x| x.window == *w).ok_or(format!("Ein Fenster {w} gibt es nicht."))?;
            if tree && never_seen(app, &x.app) {
                return Err(NEVER_SEE.into());
            }
            Some(x.rect)
        }
        None => {
            if tree && windows.iter().any(|x| x.rect[2] > 0 && never_seen(app, &x.app)) {
                return Err(format!("{NEVER_SEE} Ein Fenster davon ist zu sehen."));
            }
            None
        }
    };
    comp.sync_pixels();
    let (sw, sh) = comp.size();
    let [x0, y0, w, h] = rect.unwrap_or([0, 0, sw, sh]);
    let (x0, y0) = (x0.clamp(0, sw), y0.clamp(0, sh));
    let (w, h) = (w.min(sw - x0).max(0), h.min(sh - y0).max(0));
    if w == 0 || h == 0 {
        return Err("Das Fenster ist nicht zu sehen.".into());
    }
    let (px, stride) = comp.pixels();
    let mut buf = format!("P6\n{w} {h}\n255\n").into_bytes();
    for y in y0 as usize..(y0 + h) as usize {
        for &v in &px[y * stride + x0 as usize..y * stride + (x0 + w) as usize] {
            buf.extend_from_slice(&[(v >> 16) as u8, (v >> 8) as u8, v as u8]);
        }
    }
    write_new(&p, &buf)?;
    Ok(format!("{w}x{h}"))
}

fn write_new(p: &Path, data: &[u8]) -> Result<(), String> {
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(p).map_err(|e| format!("{}: {e}", p.display()))?;
    f.write_all(data).map_err(|e| e.to_string())
}
