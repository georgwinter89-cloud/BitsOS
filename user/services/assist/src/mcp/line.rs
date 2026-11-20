//! The two pipes of `assist --mcp`. The other end is another program: a line it sends is bounded before it is
//! kept, and a line for it is handed to a thread that may block on the pipe, so a client that sends without
//! end or never reads costs this mode its memory bound or its patience, never more.

use std::io::{Read, Write};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::{Duration, Instant};

/// Lines read ahead of the one at work. More than these wait in the pipe, which then holds the client back.
const AHEAD: usize = 32;
/// Answers waiting for the pipe; more than these and the client is not reading.
const BEHIND: usize = 8;
const CHUNK: usize = 16 * 1024;

pub enum Line {
    Text(String),
    /// A line longer than the bound: nothing of it was kept.
    TooLong,
    NotUtf8,
    /// The client closed its side (or reading failed).
    End,
}

/// Reads `input` on a thread of its own and hands on whole lines of at most `max` bytes.
pub fn reader(mut input: impl Read + Send + 'static, max: usize) -> std::io::Result<Receiver<Line>> {
    let (tx, rx) = sync_channel(AHEAD);
    std::thread::Builder::new().name("mcp-in".into()).spawn(move || {
        let mut line: Vec<u8> = Vec::new();
        // Inside a line that is already too long: its bytes are read away up to the line end.
        let mut skipping = false;
        let mut chunk = vec![0u8; CHUNK];
        let whole = |line: &mut Vec<u8>, skipping: &mut bool| {
            let out = match (std::mem::take(skipping), String::from_utf8(std::mem::take(line))) {
                (true, _) => Line::TooLong,
                (false, Ok(text)) => Line::Text(text),
                (false, Err(_)) => Line::NotUtf8,
            };
            tx.send(out).is_ok()
        };
        loop {
            let n = match input.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let mut rest = &chunk[..n];
            while !rest.is_empty() {
                let end = rest.iter().position(|b| *b == b'\n');
                let part = &rest[..end.unwrap_or(rest.len())];
                if !skipping && line.len() + part.len() <= max {
                    line.extend_from_slice(part);
                } else {
                    skipping = true;
                    line = Vec::new();
                }
                rest = &rest[part.len()..];
                if end.is_some() {
                    rest = &rest[1..];
                    if !whole(&mut line, &mut skipping) {
                        return;
                    }
                }
            }
        }
        // A last line without its line end still counts.
        if (skipping || !line.is_empty()) && !whole(&mut line, &mut skipping) {
            return;
        }
        let _ = tx.send(Line::End);
    })?;
    Ok(rx)
}

/// The client does not take what is written to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lost {
    /// It closed its side.
    Gone,
    /// It is there and does not read.
    Stuck,
}

pub struct Out {
    tx: Option<SyncSender<String>>,
    done: Receiver<()>,
    wait: Duration,
}

impl Out {
    /// Writes to `output` on a thread of its own. A line waits at most `wait` for room.
    pub fn start(mut output: impl Write + Send + 'static, wait: Duration) -> std::io::Result<Out> {
        let (tx, rx) = sync_channel::<String>(BEHIND);
        let (done_tx, done) = sync_channel(1);
        std::thread::Builder::new().name("mcp-out".into()).spawn(move || {
            for mut line in rx {
                line.push('\n');
                if output.write_all(line.as_bytes()).and_then(|_| output.flush()).is_err() {
                    return;
                }
            }
            let _ = done_tx.send(());
        })?;
        Ok(Out { tx: Some(tx), done, wait })
    }

    pub fn send(&self, mut line: String) -> Result<(), Lost> {
        let Some(tx) = &self.tx else { return Err(Lost::Gone) };
        let until = Instant::now() + self.wait;
        loop {
            line = match tx.try_send(line) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Disconnected(_)) => return Err(Lost::Gone),
                Err(TrySendError::Full(line)) => line,
            };
            if Instant::now() >= until {
                return Err(Lost::Stuck);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Waits until everything handed over is written, at most the usual wait.
    pub fn finish(mut self) -> Result<(), Lost> {
        self.tx = None;
        match self.done.recv_timeout(self.wait) {
            Ok(()) => Ok(()),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(Lost::Stuck),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(Lost::Gone),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(input: &[u8], max: usize) -> Vec<String> {
        let rx = reader(std::io::Cursor::new(input.to_vec()), max).unwrap();
        rx.iter()
            .map(|l| match l {
                Line::Text(t) => t,
                Line::TooLong => "<too long>".into(),
                Line::NotUtf8 => "<not utf-8>".into(),
                Line::End => "<end>".into(),
            })
            .collect()
    }

    #[test]
    fn lines_are_bounded_and_the_stream_goes_on_after_a_long_one() {
        let mut input = b"one\n".to_vec();
        input.extend(std::iter::repeat_n(b'x', 100_000));
        input.extend(b"\ntwo\n\xff\xfe\nlast");
        assert_eq!(lines(&input, 64), ["one", "<too long>", "two", "<not utf-8>", "last", "<end>"]);
        // Exactly at the bound is still a line; a long one at the very end is reported too.
        assert_eq!(lines(b"abcd\nabcde", 4), ["abcd", "<too long>", "<end>"]);
        assert_eq!(lines(b"", 4), ["<end>"]);
    }
}
