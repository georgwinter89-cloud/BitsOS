//! Reading a whole file into a growing buffer: no size limit, the reported size is only the first guess (a
//! FAT driver that reports a wrong size cannot make bitsboot cut a file short or read past its buffer), and a
//! file too large for memory is a readable error instead of an allocation panic.

use alloc::vec::Vec;

#[derive(Debug, PartialEq, Eq)]
pub enum ReadError<E> {
    /// Memory for `wanted` bytes could not be had.
    NoMemory { wanted: usize },
    /// The firmware's read failed after `done` bytes.
    Io { done: usize, err: E },
}

/// First and smallest step by which the buffer grows past the reported size.
const STEP: usize = 64 * 1024;

/// Reads until the reader returns 0. `size_hint` is the size the file system reported.
pub fn read_to_end<E>(size_hint: u64, mut read: impl FnMut(&mut [u8]) -> Result<usize, E>) -> Result<Vec<u8>, ReadError<E>> {
    let mut buf: Vec<u8> = Vec::new();
    // One byte more than reported, so a correct size ends with a single 0-byte read and no regrowth.
    let first = usize::try_from(size_hint).ok().and_then(|n| n.checked_add(1)).unwrap_or(usize::MAX);
    grow(&mut buf, first)?;
    let mut done = 0;
    loop {
        if done == buf.len() {
            let to = buf.len().saturating_add(buf.len().max(STEP));
            grow(&mut buf, to)?;
        }
        match read(&mut buf[done..]) {
            Ok(0) => break,
            Ok(n) => done += n.min(buf.len() - done),
            Err(err) => return Err(ReadError::Io { done, err }),
        }
    }
    buf.truncate(done);
    Ok(buf)
}

fn grow<E>(buf: &mut Vec<u8>, to: usize) -> Result<(), ReadError<E>> {
    let extra = to.saturating_sub(buf.len());
    buf.try_reserve_exact(extra).map_err(|_| ReadError::NoMemory { wanted: to })?;
    buf.resize(to, 0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;

    /// A reader over `data` that hands out at most `chunk` bytes per call.
    fn reader(data: &[u8], chunk: usize) -> impl FnMut(&mut [u8]) -> Result<usize, ()> + '_ {
        let mut pos = 0;
        move |out: &mut [u8]| {
            let n = out.len().min(chunk).min(data.len() - pos);
            out[..n].copy_from_slice(&data[pos..pos + n]);
            pos += n;
            Ok(n)
        }
    }

    #[test]
    fn exact_short_and_long_hints() {
        let data: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
        for hint in [300_000u64, 0, 10, 1 << 30 >> 10, 299_999, 300_001] {
            assert_eq!(read_to_end(hint, reader(&data, 4096)).unwrap(), data, "hint {hint}");
        }
        assert_eq!(read_to_end(0, reader(&[], 10)).unwrap(), b"");
    }

    #[test]
    fn errors_are_reported_with_progress() {
        let mut calls = 0;
        let r = read_to_end(100, |out: &mut [u8]| {
            calls += 1;
            if calls == 1 {
                out[..40].fill(1);
                Ok(40)
            } else {
                Err("device error")
            }
        });
        assert_eq!(r, Err(ReadError::Io { done: 40, err: "device error" }));
    }

    #[test]
    fn impossible_size_is_no_memory_not_a_panic() {
        let r = read_to_end(u64::MAX, reader(&[1, 2, 3], 1));
        assert!(matches!(r, Err(ReadError::NoMemory { .. })));
        let r = read_to_end((isize::MAX as u64) + 5, reader(&[1], 1));
        assert!(matches!(r, Err(ReadError::NoMemory { .. })));
    }

    #[test]
    fn a_reader_claiming_too_much_is_clamped() {
        let data = vec![7u8; 10];
        let mut first = true;
        let r = read_to_end(10, |out: &mut [u8]| {
            if first {
                first = false;
                out[..10].copy_from_slice(&data);
                Ok::<usize, ()>(usize::MAX)
            } else {
                Ok(0)
            }
        });
        assert_eq!(r.unwrap().len(), 11);
    }
}
