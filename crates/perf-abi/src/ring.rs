//! The data area's arithmetic (linux-7.3.1/kernel/events/ring_buffer.c, include/linux/circ_buf.h): `data_head`
//! and `data_tail` only grow, the area is a power of two, a record that crosses its end is written in two
//! pieces. The writer trusts nothing in `data_tail` (the program writes it): the masks keep every answer inside
//! the area.

/// `CIRC_SPACE(head, tail, size)`: bytes the writer may still put in without overtaking the reader. One byte is
/// always left free, so a full area (head - tail == size) is never confused with an empty one.
pub fn space(head: u64, tail: u64, size: u64) -> u64 {
    if size == 0 {
        return 0;
    }
    tail.wrapping_sub(head.wrapping_add(1)) & (size - 1)
}

/// Bytes waiting for the reader (`CIRC_CNT`), what a program reads between `tail` and `head`.
pub fn count(head: u64, tail: u64, size: u64) -> u64 {
    if size == 0 {
        return 0;
    }
    head.wrapping_sub(tail) & (size - 1)
}

/// Where `len` bytes written at stream position `pos` land: the offset inside the area and the length of the first
/// piece, then the length of the piece that wraps to offset 0 (0 when none).
pub fn pieces(pos: u64, len: u64, size: u64) -> (u64, u64, u64) {
    let off = pos & (size - 1);
    let first = len.min(size - off);
    (off, first, len - first)
}

/// `rb->watermark` without `wakeup_watermark`: half the area (ring_buffer.c `ring_buffer_init`).
pub fn default_watermark(size: u64) -> u64 {
    size / 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn space_and_count() {
        assert_eq!(space(0, 0, 4096), 4095);
        assert_eq!(space(4000, 0, 4096), 95);
        assert_eq!(space(8192, 4096, 4096), 4095);
        assert_eq!(count(8200, 8192, 4096), 8);
        // A tail the program spoiled still gives an answer inside the area.
        assert!(space(100, u64::MAX - 5, 4096) < 4096);
        assert_eq!(space(1, 2, 0), 0);
    }

    #[test]
    fn wrap_pieces() {
        assert_eq!(pieces(4090, 16, 4096), (4090, 6, 10));
        assert_eq!(pieces(8192, 16, 4096), (0, 16, 0));
        assert_eq!(default_watermark(8192), 4096);
    }
}
