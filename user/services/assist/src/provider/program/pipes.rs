//! The helper's two output pipes, each read on a thread of its own into the one channel the answer loop
//! waits on: a helper that never writes, or never stops, holds up its reader and nothing else.

use super::bell::Ring;
use super::calls::Call;
use crate::term::wrap;
use std::io::Read;
use std::sync::mpsc::SyncSender;

/// What the helper wrote to stderr goes to the log: this many lines of this many characters, the rest is
/// read away (a helper must never block on its stderr) and counted.
const LOG_LINES: usize = 200;
pub(super) const LOG_LINE: usize = 400;
const CHUNK: usize = 16 * 1024;

pub(super) enum Msg {
    Out(Vec<u8>),
    /// stdout is closed.
    End,
    TooBig,
    Err(String),
    /// stderr is closed.
    ErrEnd,
    /// A question came through the bell.
    Ring(Ring),
    /// A technical line about the bell or the tool socket (who was turned away).
    Bell(String),
    /// A call of one of assist's tools through the tool socket (contract version 4).
    Call(Call),
}

/// Reads the helper's stdout as lines; all of it together may take `budget` bytes.
pub(super) fn read_out(mut from: impl Read, tx: SyncSender<Msg>, budget: usize) {
    let mut line: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; CHUNK];
    let mut used = 0usize;
    loop {
        let n = match from.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        used = used.saturating_add(n);
        if used > budget {
            let _ = tx.send(Msg::TooBig);
            return;
        }
        let mut rest = &chunk[..n];
        while let Some(end) = rest.iter().position(|b| *b == b'\n') {
            line.extend_from_slice(&rest[..end]);
            if tx.send(Msg::Out(std::mem::take(&mut line))).is_err() {
                return;
            }
            rest = &rest[end + 1..];
        }
        line.extend_from_slice(rest);
    }
    if !line.is_empty() && tx.send(Msg::Out(line)).is_err() {
        return;
    }
    let _ = tx.send(Msg::End);
}

/// Reads the helper's stderr to its end, whatever comes: the first lines go to the log, the rest is counted.
pub(super) fn read_err(mut from: impl Read, tx: SyncSender<Msg>) {
    let mut line: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; CHUNK];
    let (mut lines, mut dropped, mut listening) = (0usize, 0usize, true);
    let mut whole = |line: &mut Vec<u8>, listening: &mut bool| {
        let text = String::from_utf8_lossy(line).into_owned();
        line.clear();
        if text.trim().is_empty() {
            return;
        }
        if lines < LOG_LINES && *listening {
            lines += 1;
            *listening = tx.send(Msg::Err(wrap::inline(&text, LOG_LINE))).is_ok();
        } else {
            dropped += 1;
        }
    };
    loop {
        let n = match from.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for b in &chunk[..n] {
            match b {
                b'\n' => whole(&mut line, &mut listening),
                // Beyond what is shown of a line nothing is kept.
                _ if line.len() < 4 * LOG_LINE => line.push(*b),
                _ => {}
            }
        }
    }
    whole(&mut line, &mut listening);
    if dropped > 0 && listening {
        let _ = tx.send(Msg::Err(format!("({dropped} more lines not kept)")));
    }
    let _ = tx.send(Msg::ErrEnd);
}
