//! fbtest: fbdev patterns, panning and checks (item UL-4b).
//!
//! - `fbtest info`: the mode (FBIOGET_VSCREENINFO/FSCREENINFO), opened read-only: does not take the screen.
//! - `fbtest draw SEED FRAMES [--hold SECS]`: abi-fbdev's `draw` in Rust: a double-height virtual screen when
//!   granted, FRAMES frames drawn into the hidden half and shown by FBIOPAN_DISPLAY + FBIO_WAITFORVSYNC, then a
//!   frame stored straight into the shown half and a band of rows written with pwrite. Prints
//!   `drawn a=<seed> b=<seed> band=<row>+<rows> top=<row> double=<0|1>`; pixel (x, y) is pat(a, x, y), the band
//!   rows pat(b, x, y) (the dp.fbdev.picture ktest's picture). Holds the screen SECS seconds (default 0).
//! - `fbtest check`: the ioctl rules a compositor relies on, one `ok`/`FAIL` line each (exit 1 on a failure).
//! - `fbtest demo [--seconds S]`: moving colour bars, gradient, checkerboard and pattern, double-buffered by
//!   panning, S seconds (default 5); prints the frame count and the frames per second.
//! `--dev PATH` (default /dev/fb0) for every mode. The program always ends by itself (finite holds), so the
//! console gets the screen back (V2: the screen returns when the drawing program ends).
//! Exit codes: 0 ok, 1 error or failed check, 2 usage, 4 not an fbdev device (ENOTTY).
//! Linux: linux-6.6/include/uapi/linux/fb.h, drivers/video/fbdev/core/fb_chrdev.c (ioctls), fbmem.c:727 (pan
//! bounds: EINVAL past yres_virtual - yres).
#![forbid(unsafe_code)]

mod fb;

use fb::{Fb, pat};
use std::process::exit;

const USAGE: &str = "usage: fbtest [--dev PATH] info | draw SEED FRAMES [--hold SECS] | check | demo [--seconds S]\n";

fn usage(msg: &str) -> ! {
    eprint!("fbtest: {msg}\n{USAGE}");
    exit(2)
}

fn help() -> ! {
    print!(
        "{USAGE}\n  info    show the mode (read-only open, does not take the screen)\n\
         \x20 draw    double-buffered frames + direct stores + a pwrite band; prints 'drawn a= b= band= top= double='\n\
         \x20 check   pan/var/mmap/pwrite rules, one ok/FAIL line each\n\
         \x20 demo    moving patterns for S seconds (default 5), prints frames and fps\n\
         exit codes: 0 ok, 1 error or failed check, 2 usage, 4 not an fbdev device (ENOTTY)\n"
    );
    exit(0)
}

fn num<T: std::str::FromStr>(s: Option<String>, what: &str) -> T {
    s.and_then(|v| v.parse().ok()).unwrap_or_else(|| usage(&format!("{what} wants a number")))
}

fn fail(what: &str, e: usys::Errno) -> ! {
    eprintln!("fbtest: {what}: {}", e.name());
    exit(if e.0 == 25 { 4 } else { 1 })
}

fn info(dev: &str) {
    let f = Fb::open(dev, false).unwrap_or_else(|e| fail(dev, e));
    let (v, x) = (&f.var, &f.fix);
    println!("id \"{}\"", x.id());
    println!("mode {}x{}, virtual {}x{}, offset {},{}, bpp {}", v.xres(), v.yres(), v.xres_virtual(), v.yres_virtual(), v.xoffset(), v.yoffset(), v.bpp());
    let c = |o: usize| format!("{}/{}", v.u32(o), v.u32(o + 4));
    println!("rgba {} {} {} {}, visual {}, line_length {}, smem_len {}", c(32), c(44), c(56), c(68), x.visual(), x.line_length(), x.smem_len());
    println!("panstep {} {}, wrapstep {}", x.xpanstep(), x.ypanstep(), x.ywrapstep());
}

fn hold(secs: u64) {
    for _ in 0..secs {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn draw(dev: &str, seed: u32, frames: u32, hold_s: u64) {
    let mut f = Fb::open(dev, true).unwrap_or_else(|e| fail(dev, e));
    let dbl = f.ask_double();
    let m = f.map().unwrap_or_else(|e| fail("mmap", e));
    let yres = f.var.yres();
    let mut top = 0;
    for i in 0..frames {
        let back = if dbl && top == 0 { yres } else { 0 };
        f.fill(&m, back, |x, y| pat(seed.wrapping_add(i), x, y));
        if dbl {
            f.pan(back).unwrap_or_else(|e| fail("pan", e));
        }
        top = back;
        let _ = f.vsync();
    }
    let (a, band, n) = (seed.wrapping_add(frames), yres / 3, yres / 10);
    let b = a.wrapping_add(1);
    f.fill(&m, top, |x, y| pat(a, x, y));
    f.pwrite_rows(top + band, n, |x, y| pat(b, x, band + y)).unwrap_or_else(|e| fail("pwrite", e));
    let _ = f.vsync();
    let _ = usys::fsync(f.fd);
    println!("drawn a={a} b={b} band={band}+{n} top={top} double={}", dbl as u8);
    use std::io::Write;
    let _ = std::io::stdout().flush();
    hold(hold_s);
}

fn demo(dev: &str, seconds: u64) {
    let mut f = Fb::open(dev, true).unwrap_or_else(|e| fail(dev, e));
    let dbl = f.ask_double();
    let m = f.map().unwrap_or_else(|e| fail("mmap", e));
    let (w, h) = (f.var.xres(), f.var.yres());
    let start = usys::mono_ns();
    // main refuses seconds whose nanoseconds do not fit; saturating keeps a late clock from wrapping.
    let end = start.saturating_add(seconds * 1_000_000_000);
    let (mut frames, mut top) = (0u32, 0u32);
    while usys::mono_ns() < end {
        let t = frames;
        let phase = ((usys::mono_ns() - start) / 1_250_000_000) % 4;
        let back = if dbl && top == 0 { h } else { 0 };
        f.fill(&m, back, |x, y| match phase {
            0 => {
                const BARS: [u32; 8] = [0xffffff, 0xffff00, 0x00ffff, 0x00ff00, 0xff00ff, 0xff0000, 0x0000ff, 0x000000];
                BARS[(((x + t * 8) % w) * 8 / w.max(1)) as usize % 8]
            }
            1 => ((x * 255 / w.max(1)) << 16) | ((y * 255 / h.max(1)) << 8) | (t * 4 & 0xff),
            2 => {
                if ((x + t * 4) / 64 + (y + t * 4) / 64) % 2 == 0 { 0xe0e0e0 } else { 0x202060 }
            }
            _ => pat(t, x, y),
        });
        if dbl {
            f.pan(back).unwrap_or_else(|e| fail("pan", e));
        }
        top = back;
        let _ = f.vsync();
        frames += 1;
    }
    let secs = (usys::mono_ns() - start) as f64 / 1e9;
    println!("demo frames={frames} double={} seconds={secs:.1} fps={:.1}", dbl as u8, frames as f64 / secs.max(1e-9));
}

fn main() {
    let mut dev = "/dev/fb0".to_string();
    let mut rest = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--help" | "-h" => help(),
            "--dev" => dev = it.next().unwrap_or_else(|| usage("--dev needs a path")),
            _ => rest.push(a),
        }
    }
    let mut it = rest.into_iter();
    let mode = it.next().unwrap_or_else(|| usage("which mode?"));
    let mut pos = Vec::new();
    let (mut hold_s, mut seconds) = (0u64, 5u64);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--hold" => hold_s = num(it.next(), "--hold"),
            "--seconds" => {
                seconds = num(it.next(), "--seconds");
                if seconds.checked_mul(1_000_000_000).is_none() {
                    usage("--seconds too large");
                }
            }
            _ if a.starts_with('-') => usage(&format!("unknown option {a}")),
            _ => pos.push(a),
        }
    }
    let want = match mode.as_str() {
        "draw" => 2,
        "info" | "check" | "demo" => 0,
        m => usage(&format!("unknown mode {m}")),
    };
    if pos.len() != want {
        usage(&format!("{mode} takes {want} arguments"));
    }
    let mut pos = pos.into_iter();
    match mode.as_str() {
        "info" => info(&dev),
        "draw" => {
            let seed = num(pos.next(), "SEED");
            let frames = num(pos.next(), "FRAMES");
            draw(&dev, seed, frames, hold_s)
        }
        "check" => exit(fb::check(&dev)),
        _ => demo(&dev, seconds),
    }
}
