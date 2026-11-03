//! Driver notes a Reload hands on (GX-7f CR-1, INT-W25): small records a driver writes in its Quiesce hook and the
//! next kernel of the same boot reads once at its start (amdgpu's PSP stop note, tag "GXPS").
//!
//! They live in the crash page between the crash record's body and the boot marker:
//!
//! ```text
//! len-64-area .. len-64   note slots of 64 bytes each (area = page_len / 16, whole slots)
//!   0 tag u32 (0: empty)   4 writer's Reload counter u32   8 data bytes u16   10 magic u16 0x544e ("NT")
//!   12 checksum u32: FNV-1a 32 over the boot id, bytes 0..12 and the data   16 data (at most 48 bytes)
//! ```
//!
//! The checksum includes the boot id, so a note from another boot (RAM kept across a reset) never matches. A
//! reader takes only notes whose writer's Reload counter is one below its own, the rule of `running_kernel_id`:
//! a note left by a kernel two Reloads back, or by a Reload that did not happen, is not handed on. The number of
//! slots follows the page size; a full area refuses a new tag (`false`), it never drops another driver's note.

use super::record::Fnv32;

pub const SLOT: usize = 64;
pub const HEAD: usize = 16;
/// Most data bytes per note.
pub const MAX_DATA: usize = SLOT - HEAD;
const MAGIC: u16 = 0x544e;

/// Bytes of the note area in a crash page of `page_len` (whole slots).
pub fn area(page_len: usize) -> usize {
    (page_len / 16) / SLOT * SLOT
}

/// Offset of slot `i` in a page of `page_len` (`marker` = the boot marker's size).
pub fn slot_offset(page_len: usize, marker: usize, i: usize) -> usize {
    page_len - marker - area(page_len) + i * SLOT
}

fn sum(boot_id: &[u8; 16], head: &[u8], data: &[u8]) -> u32 {
    let mut f = Fnv32::new();
    f.update(boot_id);
    f.update(&head[..12]);
    f.update(data);
    f.finish()
}

/// One intact note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Note<'a> {
    pub tag: u32,
    pub reload: u32,
    pub data: &'a [u8],
}

/// The 64 slot bytes of a note; `None` for tag 0 or more than `MAX_DATA` bytes.
pub fn encode(boot_id: &[u8; 16], tag: u32, reload: u32, data: &[u8]) -> Option<[u8; SLOT]> {
    if tag == 0 || data.len() > MAX_DATA {
        return None;
    }
    let mut b = [0u8; SLOT];
    b[0..4].copy_from_slice(&tag.to_le_bytes());
    b[4..8].copy_from_slice(&reload.to_le_bytes());
    b[8..10].copy_from_slice(&(data.len() as u16).to_le_bytes());
    b[10..12].copy_from_slice(&MAGIC.to_le_bytes());
    b[HEAD..HEAD + data.len()].copy_from_slice(data);
    let s = sum(boot_id, &b, data);
    b[12..16].copy_from_slice(&s.to_le_bytes());
    Some(b)
}

/// The note in slot bytes `b` of boot `boot_id`; `None` when empty, damaged or from another boot.
pub fn parse<'a>(boot_id: &[u8; 16], b: &'a [u8]) -> Option<Note<'a>> {
    if b.len() < SLOT {
        return None;
    }
    let tag = u32::from_le_bytes(b[0..4].try_into().ok()?);
    let reload = u32::from_le_bytes(b[4..8].try_into().ok()?);
    let n = u16::from_le_bytes(b[8..10].try_into().ok()?) as usize;
    let magic = u16::from_le_bytes(b[10..12].try_into().ok()?);
    if tag == 0 || magic != MAGIC || n > MAX_DATA {
        return None;
    }
    let data = &b[HEAD..HEAD + n];
    (sum(boot_id, b, data) == u32::from_le_bytes(b[12..16].try_into().ok()?)).then_some(Note { tag, reload, data })
}

/// Which of `n` slots a writer with Reload counter `mine` uses for `tag`, given each slot's current note as
/// (tag, Reload counter) by `at` (no allocation: the Quiesce path calls it): its own note of that tag first, else
/// the first slot that holds no note of this kernel (empty, damaged, or a kernel before's).
pub fn choose_slot(n: usize, at: impl Fn(usize) -> Option<(u32, u32)>, tag: u32, mine: u32) -> Option<usize> {
    (0..n).find(|&i| at(i) == Some((tag, mine))).or_else(|| (0..n).find(|&i| at(i).is_none_or(|(_, r)| r != mine)))
}

/// A reader with Reload counter `mine` takes a note only from the kernel one Reload before.
pub fn from_last(note_reload: u32, mine: u32) -> bool {
    mine > 0 && note_reload.checked_add(1) == Some(mine)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: [u8; 16] = [7; 16];

    #[test]
    fn a_note_round_trips_and_binds_to_its_boot() {
        let b = encode(&ID, u32::from_le_bytes(*b"GXPS"), 3, &[1, 2, 3]).unwrap();
        let n = parse(&ID, &b).unwrap();
        assert_eq!((n.tag, n.reload, n.data), (u32::from_le_bytes(*b"GXPS"), 3, &[1u8, 2, 3][..]));
        assert!(parse(&[8; 16], &b).is_none(), "another boot's note");
        let mut d = b;
        d[HEAD] ^= 1;
        assert!(parse(&ID, &d).is_none(), "damaged data");
        assert!(parse(&ID, &[0u8; SLOT]).is_none(), "empty slot");
        assert!(encode(&ID, 0, 1, &[]).is_none());
        assert!(encode(&ID, 1, 1, &[0; MAX_DATA + 1]).is_none());
        let full = encode(&ID, 1, 1, &[0xAB; MAX_DATA]).unwrap();
        assert_eq!(parse(&ID, &full).unwrap().data, &[0xAB; MAX_DATA][..]);
    }

    #[test]
    fn the_area_follows_the_page_and_sits_below_the_marker() {
        assert_eq!(area(4096), 256);
        assert_eq!(area(1024), 64);
        assert_eq!(slot_offset(4096, 64, 0), 4096 - 64 - 256);
        assert_eq!(slot_offset(4096, 64, 3) + SLOT, 4096 - 64);
    }

    #[test]
    fn a_writer_replaces_its_own_tag_and_keeps_other_drivers_notes() {
        let a = 0x41;
        let b = 0x42;
        let choose_slot = |s: &[Option<(u32, u32)>], tag, mine| choose_slot(s.len(), |i| s[i], tag, mine);
        // Own note of the tag: replaced in place.
        assert_eq!(choose_slot(&[Some((b, 2)), Some((a, 2))], a, 2), Some(1));
        // A kernel before's notes and empty slots are free; this kernel's other tag is kept.
        assert_eq!(choose_slot(&[Some((b, 2)), Some((a, 1)), None], a, 2), Some(1));
        assert_eq!(choose_slot(&[Some((b, 2)), None], a, 2), Some(1));
        assert_eq!(choose_slot(&[Some((b, 2)), Some((a + 2, 2))], a, 2), None, "full: refused");
    }

    #[test]
    fn only_the_kernel_one_reload_before_hands_on() {
        assert!(from_last(0, 1));
        assert!(from_last(4, 5));
        assert!(!from_last(3, 5), "two Reloads back");
        assert!(!from_last(5, 5), "this kernel's own (a Reload that did not happen)");
        assert!(!from_last(0, 0), "a firmware start");
        assert!(!from_last(u32::MAX, 0));
    }
}
