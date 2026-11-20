//! GS-1: the marks of the desktop's way to its first picture (plan/spec/bitscomp.md §1 "Start marks"). Each mark is
//! a line `bitscomp: boot.mark stage=<s> at_ms=<ms>` on stderr and in the kernel log (`/dev/kmsg`, event
//! `user kmsg`); `at_ms` is CLOCK_MONOTONIC, the kernel log's own clock. The first picture says it all in one line,
//! `boot.desktop_ready ms=<ms> renderer=<r> ...`, with the kernel's marks (`/proc/bitsos/boot` `mark` lines) and this
//! process's (`start`, `renderer_up`, `frame`); `bitscomp ctl boot` answers it.
//!
//! `/dev/kmsg` is root's to write: it is opened at the start, while the console's root start still is root, and kept
//! (close-on-exec) after the drop to the desktop user.

use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// The kernel's step list with its marks.
const KERNEL: &str = "/proc/bitsos/boot";

static KMSG: Mutex<Option<std::fs::File>> = Mutex::new(None);
static T0_MS: AtomicU64 = AtomicU64::new(0);
static MARKS: Mutex<Vec<(String, u64)>> = Mutex::new(Vec::new());
static SUMMARY: Mutex<Option<String>> = Mutex::new(None);
static PRESENTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn now_ms() -> u64 {
    crate::pacer::now().as_millis() as u64
}

/// First in `main`, while bitscomp may be root.
pub fn begin() {
    let t0 = now_ms();
    T0_MS.store(t0, Ordering::Release);
    *lock(&KMSG) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg").ok();
    lock(&MARKS).push(("start".to_string(), t0));
}

/// This process's start on the monotonic clock (ms).
pub fn started_ms() -> u64 {
    T0_MS.load(Ordering::Acquire)
}

/// A mark reached, now.
pub fn mark(stage: &str) {
    mark_at(stage, now_ms());
}

pub fn mark_at(stage: &str, at_ms: u64) {
    lock(&MARKS).push((stage.to_string(), at_ms));
    say(&format!("boot.mark stage={stage} at_ms={at_ms}"));
}

pub fn get(stage: &str) -> Option<u64> {
    lock(&MARKS).iter().find(|(s, _)| s == stage).map(|(_, t)| *t)
}

/// The marks so far, for `ctl boot`: `name=ms,name=ms`.
pub fn marks_value() -> String {
    lock(&MARKS).iter().map(|(s, t)| format!("{s}={t}")).collect::<Vec<_>>().join(",")
}

/// After every present with damage: the first one is the desktop's first picture (the mark and the summary).
pub fn presented(renderer: &str) {
    if PRESENTED.swap(true, Ordering::AcqRel) {
        return;
    }
    mark("frame");
    summary(renderer);
}

/// The `mark <name> <ms>` lines of the kernel's step list, in their order.
pub fn kernel_marks(text: &str) -> Vec<(String, u64)> {
    text.lines()
        .filter_map(|l| l.strip_prefix("mark "))
        .filter_map(|l| {
            let (n, t) = l.split_once(' ')?;
            Some((n.to_string(), t.trim().parse().ok()?))
        })
        .collect()
}

/// The summary line: the first picture's time and the renderer, then every mark in time order.
pub fn summary_line(frame_ms: u64, renderer: &str, kernel: &[(String, u64)], own: &[(String, u64)]) -> String {
    let mut all: Vec<&(String, u64)> = kernel.iter().chain(own.iter()).collect();
    all.sort_by_key(|(_, t)| *t);
    let mut s = format!("boot.desktop_ready ms={frame_ms} renderer={renderer}");
    for (n, t) in all {
        s.push_str(&format!(" {n}={t}"));
    }
    s
}

/// Once per process, at its first picture.
fn summary(renderer: &str) {
    if lock(&SUMMARY).is_some() {
        return;
    }
    let frame = get("frame").unwrap_or_else(now_ms);
    let kernel = kernel_marks(&std::fs::read_to_string(KERNEL).unwrap_or_default());
    let own = lock(&MARKS).clone();
    let line = summary_line(frame, renderer, &kernel, &own);
    say(&line);
    *lock(&SUMMARY) = Some(line);
}

/// `bitscomp ctl boot`: the summary (or `boot none` before the first frame), then this process's marks so far
/// (`shown` comes after the summary).
pub fn ctl(words: &[&str]) -> Option<Result<String, String>> {
    if words.first() != Some(&"boot") {
        return None;
    }
    let head = lock(&SUMMARY).clone().unwrap_or_else(|| String::from("boot none"));
    Some(Ok(format!("{head}\nboot.marks {}", marks_value())))
}

/// A line of the start for stderr and the kernel log (one record each, never torn by other output).
pub fn say(text: &str) {
    eprintln!("bitscomp: {text}");
    if let Some(f) = lock(&KMSG).as_mut() {
        // One write, one record (crates/vfs-sys kmsg.rs): `<6>` is Info.
        let _ = f.write_all(format!("<6>bitscomp: {text}\n").as_bytes());
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(p: &[(&str, u64)]) -> Vec<(String, u64)> {
        p.iter().map(|(n, t)| (n.to_string(), *t)).collect()
    }

    #[test]
    fn the_summary_names_every_stage_in_time_order_and_fits_one_record() {
        let k = kernel_marks("bitsos-boot 1\nseq 9\nstep gpu done 3100 gpu bereit\nmark gfx_start 1200\nmark engines 2900\nmark node 3100\nmark desktop_start 3300\nmark bad\nmark x y\n");
        assert_eq!(k, v(&[("gfx_start", 1200), ("engines", 2900), ("node", 3100), ("desktop_start", 3300)]));
        let own = v(&[("start", 3350), ("renderer_up", 4100), ("frame", 4400)]);
        let s = summary_line(4400, "gles", &k, &own);
        assert_eq!(
            s,
            "boot.desktop_ready ms=4400 renderer=gles gfx_start=1200 engines=2900 node=3100 desktop_start=3300 start=3350 renderer_up=4100 frame=4400"
        );
        // The kernel cuts a record at 240 bytes: the longest line (six digits each, the longest words) stays one.
        let big: Vec<(String, u64)> = k.iter().chain(own.iter()).map(|(n, _)| (n.clone(), 999_999)).collect();
        let long = format!("bitscomp: {}", summary_line(999_999, "gles", &big, &[]));
        assert!(long.len() <= 240, "{} bytes: {long}", long.len());
    }
}
