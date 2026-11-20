use crate::attach::*;
use crate::layout::{MEMSIZE_MIB_SEEN, MemsizeError};
use std::vec::Vec;

const BAR0: u64 = 0xA8_0000_0000;
const BAR0_BYTES: u64 = 16 << 30;

/// Answers the two dwords and logs everything. Any other dword is a test failure: this step reads these two and no
/// more. The mailbox's three dwords answer only for a fake that was given them.
struct Fake {
    ready_after: u32,
    c2pmsg_33: u32,
    memsize: u32,
    /// DRIVER_SCRATCH_0..2.
    scratch: Option<[u32; 3]>,
    reads: Vec<u32>,
    slept_ms: u32,
}

impl Fake {
    fn new(ready_after: u32, memsize: u32) -> Self {
        Fake { ready_after, c2pmsg_33: IFWI_READY, memsize, scratch: None, reads: Vec::new(), slept_ms: 0 }
    }

    fn mailbox(lo: u32, hi: u32, size: u32) -> Self {
        Fake { scratch: Some([lo, hi, size]), ..Fake::new(0, MEMSIZE_MIB_SEEN) }
    }
}

impl Regs for Fake {
    fn read_dword(&mut self, dword: u32) -> u32 {
        self.reads.push(dword);
        match dword {
            C2PMSG_33_DWORD if self.slept_ms >= self.ready_after => self.c2pmsg_33,
            C2PMSG_33_DWORD => 0,
            MEMSIZE_DWORD => self.memsize,
            DRIVER_SCRATCH_0_DWORD..=DRIVER_SCRATCH_2_DWORD if self.scratch.is_some() => {
                self.scratch.unwrap()[(dword - DRIVER_SCRATCH_0_DWORD) as usize]
            }
            other => panic!("read of dword {other:#x}"),
        }
    }

    fn sleep_ms(&mut self, ms: u32) {
        self.slept_ms += ms;
    }
}

#[test]
fn a_ready_card_gives_the_place_of_its_blob() {
    let mut fake = Fake::new(0, MEMSIZE_MIB_SEEN);
    let got = locate_discovery(&mut fake, BAR0, BAR0_BYTES).unwrap();
    assert_eq!(got, Located { memsize_mib: 16_304, vram_bytes: 17_095_983_104, discovery: 0xAB_FAFF_0000 });
    assert_eq!(fake.reads, [C2PMSG_33_DWORD, MEMSIZE_DWORD]);
    assert_eq!(fake.slept_ms, 0);
}

#[test]
fn it_waits_up_to_two_seconds_a_millisecond_at_a_time() {
    let mut fake = Fake::new(100, MEMSIZE_MIB_SEEN);
    assert!(locate_discovery(&mut fake, BAR0, BAR0_BYTES).is_ok());
    assert_eq!(fake.slept_ms, 100);

    let mut fake = Fake::new(1999, MEMSIZE_MIB_SEEN);
    assert!(locate_discovery(&mut fake, BAR0, BAR0_BYTES).is_ok());
}

#[test]
fn a_card_that_never_gets_ready_is_left_alone() {
    let mut fake = Fake::new(u32::MAX, MEMSIZE_MIB_SEEN);
    assert_eq!(locate_discovery(&mut fake, BAR0, BAR0_BYTES), Err(AttachError::NotReady));
    assert_eq!(fake.slept_ms, IFWI_WAIT_MS);
    assert!(!fake.reads.contains(&MEMSIZE_DWORD), "no read past the wait");
}

/// A dead link reads all ones, which has the ready bit set, so it is the memory size that catches it.
#[test]
fn a_card_that_is_gone_is_named_as_gone() {
    let mut fake = Fake::new(0, u32::MAX);
    fake.c2pmsg_33 = u32::MAX;
    let want = AttachError::Memsize(MemsizeError::Gone);
    assert_eq!(locate_discovery(&mut fake, BAR0, BAR0_BYTES), Err(want));
}

#[test]
fn a_card_with_no_memory_size_or_too_small_a_window_is_refused() {
    let mut fake = Fake::new(0, 0);
    let want = AttachError::Memsize(MemsizeError::Zero);
    assert_eq!(locate_discovery(&mut fake, BAR0, BAR0_BYTES), Err(want));

    let mut fake = Fake::new(0, MEMSIZE_MIB_SEEN);
    assert_eq!(locate_discovery(&mut fake, BAR0, 256 << 20), Err(AttachError::OutsideBar0));
}

#[test]
fn the_fixed_numbers_are_the_tables_numbers() {
    use crate::ipfacts::TABLE;
    use crate::regmap::{MEMSIZE, PSP_C2PMSG_33, dword};
    assert_eq!(dword(&TABLE, PSP_C2PMSG_33), Some(C2PMSG_33_DWORD));
    assert_eq!(dword(&TABLE, MEMSIZE), Some(MEMSIZE_DWORD));
}

/// Linux's second place (A:`amdgpu_discovery.c`:338-349): the size dword first, and only a non-zero size reads the
/// offset.
#[test]
fn the_mailbox_names_another_place_or_none() {
    let mut none = Fake::mailbox(0x1234_0000, 3, 0);
    assert_eq!(mailbox_discovery(&mut none, BAR0_BYTES), Ok(None));
    assert_eq!(none.reads, [DRIVER_SCRATCH_2_DWORD], "a size of 0 reads no offset");

    // 0x3_F000_0000: 64-bit, low and high dword.
    let mut fake = Fake::mailbox(0xF000_0000, 3, 0x2800);
    assert_eq!(mailbox_discovery(&mut fake, BAR0_BYTES), Ok(Some(Mailbox { offset: 0x3_F000_0000, size: 0x2800, read: 0x2800 })));
    assert_eq!(fake.reads, [DRIVER_SCRATCH_2_DWORD, DRIVER_SCRATCH_0_DWORD, DRIVER_SCRATCH_1_DWORD]);

    // A region larger than any table: its first 64 KiB are read (binary_size is 16 bits), the whole must fit BAR0.
    let mut big = Fake::mailbox(0x10_0000, 0, 1 << 20);
    assert_eq!(mailbox_discovery(&mut big, BAR0_BYTES), Ok(Some(Mailbox { offset: 0x10_0000, size: 1 << 20, read: 64 << 10 })));
    // Up to the last byte of BAR0, and not one past it.
    let last = (BAR0_BYTES - 0x2800) as u32;
    let hi = ((BAR0_BYTES - 0x2800) >> 32) as u32;
    assert!(mailbox_discovery(&mut Fake::mailbox(last, hi, 0x2800), BAR0_BYTES).is_ok_and(|m| m.is_some()));
    assert_eq!(
        mailbox_discovery(&mut Fake::mailbox(last + 4, hi, 0x2800), BAR0_BYTES),
        Err(MailboxRefusal::OutsideBar0 { offset: BAR0_BYTES - 0x2800 + 4, size: 0x2800 })
    );
}

#[test]
fn a_mailbox_place_that_cannot_be_read_safely_is_refused_by_name() {
    assert_eq!(
        mailbox_discovery(&mut Fake::mailbox(0, 0x10, 0x2800), BAR0_BYTES),
        Err(MailboxRefusal::OutsideBar0 { offset: 0x10_0000_0000, size: 0x2800 })
    );
    assert_eq!(
        mailbox_discovery(&mut Fake::mailbox(0xFFFF_FFFC, 0xFFFF_FFFF, 0x2800), BAR0_BYTES),
        Err(MailboxRefusal::OutsideBar0 { offset: 0xFFFF_FFFF_FFFF_FFFC, size: 0x2800 }),
        "an offset whose end wraps"
    );
    assert_eq!(mailbox_discovery(&mut Fake::mailbox(0x1000, 0, 8), BAR0_BYTES), Err(MailboxRefusal::Size { size: 8 }));
    assert_eq!(
        mailbox_discovery(&mut Fake::mailbox(0x1002, 0, 0x2800), BAR0_BYTES),
        Err(MailboxRefusal::Unaligned { offset: 0x1002, size: 0x2800 })
    );
    assert_eq!(
        mailbox_discovery(&mut Fake::mailbox(0x1000, 0, 0x2801), BAR0_BYTES),
        Err(MailboxRefusal::Unaligned { offset: 0x1000, size: 0x2801 })
    );
    // A card that stopped answering: the size dword's all ones is enough, the offset is not read.
    let mut gone = Fake::mailbox(u32::MAX, u32::MAX, u32::MAX);
    assert_eq!(mailbox_discovery(&mut gone, BAR0_BYTES), Err(MailboxRefusal::Gone));
    assert_eq!(gone.reads, [DRIVER_SCRATCH_2_DWORD]);
}
