//! `bits foto [--fenster W] <datei>`: a picture of the screen or of one window. bitscomp writes it (`bits-foto`, the
//! window cut out there, so a program the assistant may not see is refused there), `bits` turns it into PNG
//! (`.png`, also for any other ending) or keeps the PPM (`.ppm`).

use crate::out::{Done, Fail, R};
use crate::{png, sock, ui};

pub fn run(args: &[String]) -> R {
    let mut window = None;
    let mut file = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--fenster" => window = Some(ui::c1_window(it.next().ok_or_else(|| Fail::usage("--fenster braucht ein Fenster (so: w3)"))?)),
            o if o.starts_with("--") => return Err(Fail::usage(format!("foto kennt „{o}“ nicht"))),
            f => file = Some(f.to_string()),
        }
    }
    let file = file.ok_or_else(|| Fail::usage("foto [--fenster w3] <datei.png>"))?;
    let tmp = sock::runtime_dir().join(format!(".bits-foto-{}-{}.ppm", std::process::id(), crate::clip::nanos()));
    let line = match &window {
        Some(w) => format!("bits-foto {} {w}", tmp.display()),
        None => format!("bits-foto {}", tmp.display()),
    };
    let r = sock::ctl(&line).and_then(|_| convert(&tmp, &file));
    let _ = std::fs::remove_file(&tmp);
    let (w, h) = r?;
    Ok(Done::new(format!("{file}: {w}x{h}")).with("file", file.as_str()).with("width", w).with("height", h))
}

fn convert(tmp: &std::path::Path, file: &str) -> R<(u32, u32)> {
    let data = std::fs::read(tmp).map_err(|e| Fail::failed(format!("Das Bild kam nicht an: {e}")))?;
    let (w, h, px) = png::read_ppm(&data).map_err(Fail::failed)?;
    let out = if file.ends_with(".ppm") { data.clone() } else { png::png(w, h, px) };
    std::fs::write(file, out).map_err(|e| Fail::failed(format!("{file}: {e}")))?;
    Ok((w, h))
}
