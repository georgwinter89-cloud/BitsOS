//! L7-HW: an SMN read of all ones is an error, as linux-7.3.1/arch/x86/kernel/amd_node.c:119-128 makes it.

use super::fake::{Fake, FakeFn};
use crate::{smn_read_on, ConfigAccess, Loc};

#[test]
fn all_ones_from_the_data_register_is_no_value() {
    let fake = Fake::new();
    let mut root = FakeFn::new(0x1022, 0x14e8, 0x06_00_00, 0);
    root.w32(0x64, 0x0c80_0000);
    fake.add(0, 0, 0, root);
    let at = Loc::new(0, 0, 0, 0);
    assert_eq!(smn_read_on(&fake, at, 0x59800), Some(0x0c80_0000));
    assert_eq!(fake.read32(at, 0x60), 0x59800, "the index register holds the address");
    // A dead SMN (or a gone root) reads all ones: Linux answers -ENODEV and 0, V2 no value.
    fake.write32(at, 0x64, u32::MAX);
    assert_eq!(smn_read_on(&fake, at, 0x59800), None);
    // Nothing at the root's place: the index write goes nowhere and the read is all ones.
    assert_eq!(smn_read_on(&fake, Loc::new(0, 0, 1, 0), 0x59800), None);
}
