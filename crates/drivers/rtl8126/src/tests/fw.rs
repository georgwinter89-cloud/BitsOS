//! Synthetic scripts only: the layout is typed from `F:32-38`; no byte comes from a real firmware file.

use super::fake::Fake;
use crate::fwscript::*;
use crate::ocp::{Mdio, OcpError, STD_PHY_BASE};
use std::vec;
use std::vec::Vec;

const fn act(op: u32, regno: u32, data: u32) -> u32 {
    (op << 28) | (regno << 16) | data
}

const READ: u32 = 0x0;
const OR: u32 = 0x1;
const AND: u32 = 0x2;
const BJMPN: u32 = 0x3;
const CHG: u32 = 0x4;
const CLEAR: u32 = 0x7;
const WRITE: u32 = 0x8;
const COUNT_SKIP: u32 = 0x9;
const EQ_SKIP: u32 = 0xa;
const NEQ_SKIP: u32 = 0xb;
const WRITE_PREV: u32 = 0xc;
const SKIP: u32 = 0xd;
const DELAY: u32 = 0xe;

fn file(actions: &[u32]) -> Vec<u8> {
    let mut f = vec![0u8; 48];
    f[4..14].copy_from_slice(b"synthetic\0");
    f[36..40].copy_from_slice(&48u32.to_le_bytes());
    f[40..44].copy_from_slice(&(actions.len() as u32).to_le_bytes());
    for a in actions {
        f.extend_from_slice(&a.to_le_bytes());
    }
    seal(&mut f);
    f
}

fn seal(f: &mut [u8]) {
    f[44] = 0;
    let sum = f.iter().fold(0u8, |s, b| s.wrapping_add(*b));
    f[44] = sum.wrapping_neg();
}

#[derive(Default)]
struct Rec {
    reads: Vec<(bool, u16)>,
    writes: Vec<(bool, u16, u16)>,
    delays: Vec<u32>,
    answer: u16,
    fail: bool,
}

impl FwBus for Rec {
    fn read(&mut self, mcu: bool, reg: u16) -> Result<u16, OcpError> {
        if self.fail {
            return Err(OcpError::Timeout(1));
        }
        self.reads.push((mcu, reg));
        Ok(self.answer)
    }
    fn write(&mut self, mcu: bool, reg: u16, data: u16) -> Result<(), OcpError> {
        self.writes.push((mcu, reg, data));
        Ok(())
    }
    fn delay_ms(&mut self, ms: u32) {
        self.delays.push(ms);
    }
}

fn play(actions: &[u32], answer: u16) -> Rec {
    let f = file(actions);
    let mut rec = Rec { answer, ..Rec::default() };
    Script::parse(&f).unwrap().play(&mut rec).unwrap();
    rec
}

#[test]
fn header_fields() {
    let f = file(&[act(WRITE, 1, 2)]);
    let s = Script::parse(&f).unwrap();
    assert_eq!(s.version(), b"synthetic");
    assert_eq!(s.start(), 48);
    assert_eq!(s.len(), 1);
    assert!(!s.is_empty());
}

#[test]
fn an_empty_program_is_valid_and_does_nothing() {
    let f = file(&[]);
    let s = Script::parse(&f).unwrap();
    assert!(s.is_empty());
    let mut rec = Rec::default();
    assert_eq!(s.play(&mut rec), Ok(Played::default()));
}

#[test]
fn an_unterminated_version_keeps_31_bytes_like_strscpy() {
    let mut f = file(&[]);
    f[4..36].fill(b'x');
    seal(&mut f);
    assert_eq!(Script::parse(&f).unwrap().version().len(), 31);
}

#[test]
fn malformed_headers_are_refused() {
    assert_eq!(Script::parse(&[]).unwrap_err(), FwError::TooShort);
    assert_eq!(Script::parse(&[0, 0, 0]).unwrap_err(), FwError::TooShort);
    assert_eq!(Script::parse(&[0; 44]).unwrap_err(), FwError::TooShort);
    assert_eq!(Script::parse(&[1, 0, 0, 0]).unwrap_err(), FwError::OldFormat);

    let mut bad_sum = file(&[]);
    bad_sum[44] = bad_sum[44].wrapping_add(1);
    assert_eq!(Script::parse(&bad_sum).unwrap_err(), FwError::Checksum(1));

    let mut far = file(&[]);
    far[36..40].copy_from_slice(&49u32.to_le_bytes());
    seal(&mut far);
    assert_eq!(Script::parse(&far).unwrap_err(), FwError::StartOutOfFile(49));

    let mut in_header = file(&[]);
    in_header[36..40].copy_from_slice(&44u32.to_le_bytes());
    seal(&mut in_header);
    assert_eq!(Script::parse(&in_header).unwrap_err(), FwError::StartOutOfFile(44));

    let mut long = file(&[act(WRITE, 0, 0)]);
    long[40..44].copy_from_slice(&2u32.to_le_bytes());
    seal(&mut long);
    assert_eq!(Script::parse(&long).unwrap_err(), FwError::LenOutOfFile(2));

    let mut huge = file(&[]);
    huge[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
    seal(&mut huge);
    assert_eq!(Script::parse(&huge).unwrap_err(), FwError::LenOutOfFile(u32::MAX));
}

#[test]
fn a_flipped_bit_anywhere_is_caught_by_the_checksum() {
    let f = file(&[act(WRITE, 1, 2), act(DELAY, 0, 1)]);
    for i in 0..f.len() {
        let mut g = f.clone();
        g[i] ^= 0x10;
        assert!(Script::parse(&g).is_err(), "byte {i}");
    }
}

#[test]
fn unknown_opcodes_are_refused() {
    for op in [5, 6, 0xf] {
        let action = act(op, 0, 0);
        assert_eq!(Script::parse(&file(&[action])).unwrap_err(), FwError::BadOpcode { index: 0, action });
    }
}

#[test]
fn jumps_out_of_the_program_are_refused() {
    let cases = [
        (vec![act(CHG, 0, 2)], 0),
        (vec![act(BJMPN, 1, 0)], 0),
        (vec![act(WRITE, 0, 0), act(BJMPN, 2, 0)], 1),
        (vec![act(COUNT_SKIP, 0, 0), act(WRITE, 0, 0)], 0),
        (vec![act(EQ_SKIP, 1, 0), act(WRITE, 0, 0)], 0),
        (vec![act(NEQ_SKIP, 1, 0), act(WRITE, 0, 0)], 0),
        (vec![act(SKIP, 1, 0), act(WRITE, 0, 0)], 0),
        (vec![act(SKIP, 0xfff, 0)], 0),
    ];
    for (program, index) in cases {
        let action = program[index];
        assert_eq!(Script::parse(&file(&program)).unwrap_err(), FwError::OutOfRange { index, action });
    }
}

#[test]
fn the_edges_of_the_validator_are_accepted() {
    assert!(Script::parse(&file(&[act(WRITE, 0, 0), act(BJMPN, 1, 0)])).is_ok());
    assert!(Script::parse(&file(&[act(COUNT_SKIP, 0, 9), act(WRITE, 0, 0), act(WRITE, 0, 0)])).is_ok());
    assert!(Script::parse(&file(&[act(SKIP, 1, 0), act(WRITE, 0, 0), act(WRITE, 0, 0)])).is_ok());
    assert!(Script::parse(&file(&[act(CHG, 0, 1)])).is_ok());
}

#[test]
fn write_reaches_the_phy_until_mdio_chg_turns_to_the_mcu() {
    let rec = play(&[act(WRITE, 0x1f, 0xa43), act(CHG, 0, 1), act(WRITE, 2, 3), act(CHG, 0, 0), act(WRITE, 4, 5)], 0);
    assert_eq!(rec.writes, [(false, 0x1f, 0xa43), (true, 2, 3), (false, 4, 5)]);
}

#[test]
fn read_or_and_write_previous_compute_on_the_read_value() {
    let rec = play(&[act(READ, 7, 0), act(OR, 0, 0x000f), act(AND, 0, 0x0ff0), act(WRITE_PREV, 8, 0)], 0x1234);
    assert_eq!(rec.reads, [(false, 7)]);
    assert_eq!(rec.writes, [(false, 8, 0x0230)]);
}

#[test]
fn compare_skips() {
    let program = |op| [act(READ, 0, 0), act(op, 1, 5), act(WRITE, 1, 0), act(WRITE, 2, 0)];
    assert_eq!(play(&program(EQ_SKIP), 5).writes, [(false, 2, 0)]);
    assert_eq!(play(&program(EQ_SKIP), 6).writes, [(false, 1, 0), (false, 2, 0)]);
    assert_eq!(play(&program(NEQ_SKIP), 6).writes, [(false, 2, 0)]);
    assert_eq!(play(&program(NEQ_SKIP), 5).writes, [(false, 1, 0), (false, 2, 0)]);
}

#[test]
fn skipn_skips_regno_actions() {
    let rec = play(&[act(SKIP, 2, 0), act(WRITE, 1, 0), act(WRITE, 2, 0), act(WRITE, 3, 0)], 0);
    assert_eq!(rec.writes, [(false, 3, 0)]);
}

#[test]
fn readcount_skip_counts_reads_and_clear_resets_it() {
    let hit = [act(READ, 0, 0), act(COUNT_SKIP, 0, 1), act(WRITE, 1, 0), act(WRITE, 2, 0)];
    assert_eq!(play(&hit, 0).writes, [(false, 2, 0)]);
    let cleared = [act(READ, 0, 0), act(CLEAR, 0, 0), act(COUNT_SKIP, 0, 1), act(WRITE, 1, 0), act(WRITE, 2, 0)];
    assert_eq!(play(&cleared, 0).writes, [(false, 1, 0), (false, 2, 0)]);
}

#[test]
fn bjmpn_goes_back_regno_actions_and_a_wait_loop_ends() {
    // The shape of a wait: read, leave when the count is reached, jump back. The jump lands on the first action,
    // where Linux's unsigned index wraps for a moment.
    let program = [
        act(READ, 9, 0),
        act(COUNT_SKIP, 0, 3),
        act(BJMPN, 2, 0),
        act(WRITE, 1, 0),
    ];
    let rec = play(&program, 0);
    assert_eq!(rec.reads.len(), 3);
    assert_eq!(rec.writes, [(false, 1, 0)]);
}

#[test]
fn delays_are_passed_on() {
    assert_eq!(play(&[act(DELAY, 0, 20), act(DELAY, 0, 1)], 0).delays, [20, 1]);
}

#[test]
fn a_loop_that_never_ends_is_stopped() {
    let f = file(&[act(WRITE, 0, 0), act(BJMPN, 1, 0)]);
    let mut rec = Rec::default();
    let s = Script::parse(&f).unwrap();
    assert_eq!(s.play(&mut rec), Err(FwError::Runaway));
    assert!(rec.writes.len() as u32 <= MAX_STEPS);
}

#[test]
fn too_much_sleep_is_stopped_before_it_is_slept() {
    let f = file(&[act(DELAY, 0, 0xffff)]);
    let mut rec = Rec::default();
    assert_eq!(Script::parse(&f).unwrap().play(&mut rec), Err(FwError::TooMuchDelay));
    assert!(rec.delays.is_empty());
}

#[test]
fn a_failed_read_stops_the_script_before_the_next_write() {
    let f = file(&[act(READ, 0, 0), act(WRITE_PREV, 1, 0)]);
    let mut rec = Rec { fail: true, ..Rec::default() };
    assert_eq!(Script::parse(&f).unwrap().play(&mut rec), Err(FwError::Bus(OcpError::Timeout(1))));
    assert!(rec.writes.is_empty());
}

#[test]
fn apply_goes_through_the_page_logic_and_resets_the_page() {
    let f = file(&[act(WRITE, 0x1f, 0x0a43), act(WRITE, 0x10, 0x0abc), act(CHG, 0, 1), act(WRITE, 0x1f, 0x0e40)]);
    let hw = Fake::new();
    let mut mdio = Mdio::default();
    let played = apply(&Script::parse(&f).unwrap(), &hw, &mut mdio).unwrap();
    assert_eq!(played.writes, 3);
    assert_eq!(hw.s.borrow().phy_ocp[&0xa430], 0x0abc);
    assert_eq!(mdio.base(), STD_PHY_BASE);
}

#[test]
fn apply_resets_the_page_even_when_the_script_fails() {
    let f = file(&[act(WRITE, 0x1f, 0x0fff), act(WRITE, 0xfff, 1)]);
    let hw = Fake::new();
    let mut mdio = Mdio::default();
    assert!(matches!(apply(&Script::parse(&f).unwrap(), &hw, &mut mdio), Err(FwError::Bus(OcpError::BadAddress(_)))));
    assert_eq!(mdio.base(), STD_PHY_BASE);
}

#[test]
fn a_script_write_to_an_odd_mcu_address_stops_it() {
    let f = file(&[act(CHG, 0, 1), act(WRITE, 0x1f, 0x0e40), act(WRITE, 0x13, 1), act(WRITE, 0x12, 1)]);
    let hw = Fake::new();
    let mut mdio = Mdio::default();
    let result = apply(&Script::parse(&f).unwrap(), &hw, &mut mdio);
    assert_eq!(result, Err(FwError::Bus(OcpError::BadAddress(0xe413))));
    assert!(!hw.s.borrow().mac_ocp.contains_key(&0xe412));
}
