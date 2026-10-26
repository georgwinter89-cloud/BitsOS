//! One descriptor per class that tests/oracle.rs leaves out because V2 reads it differently from
//! V1 on purpose (plan/notes/IN-1.md): what V2 does there, asserted.

mod common;

use hidparse::{ReportDescriptor, parse};

fn desc(s: &str) -> ReportDescriptor {
    parse(&common::json::hex(
        &s.split_whitespace().collect::<String>(),
    ))
    .unwrap()
}

#[test]
fn usage_page_change_between_usages() {
    // Button 1, then page Generic Desktop, X. V1 pages both with the page at the main item
    // (0x10001, 0x10030); Linux keeps each usage's own page.
    let d = desc("05 09 09 01 05 01 09 30 75 08 95 02 81 02");
    let f = &d.reports[0].fields[0];
    assert_eq!((f.usage(0), f.usage(1)), (Some(0x9_0001), Some(0x1_0030)));
    // A page after all usages still re-pages the trailing ones (hid_concatenate_last_usage_page).
    let d = desc("05 09 09 01 09 02 05 01 75 08 95 02 81 02");
    let f = &d.reports[0].fields[0];
    assert_eq!((f.usage(0), f.usage(1)), (Some(0x1_0001), Some(0x1_0002)));
}

#[test]
fn usage_minimum_and_maximum_of_different_widths() {
    // 4-byte Minimum (page 9, button 1), short Maximum 3: Linux counts raw 0x90001..=3, which
    // is empty, so the field has no usages (padding). V1 paged both and saw buttons 1-3.
    let d = desc("05 09 1b 01 00 09 00 29 03 75 01 95 03 81 02");
    assert!(d.reports[0].fields[0].is_padding());
    // Short Minimum 1, 4-byte Maximum 0x00000003: counted raw and never re-paged (page 0).
    let d = desc("05 09 19 01 2b 03 00 00 00 75 01 95 03 81 02");
    let f = &d.reports[0].fields[0];
    assert_eq!((f.usage(0), f.usage(2)), (Some(0x0001), Some(0x0003)));
    // Both 4 bytes: the usages carry their own page.
    let d = desc("05 01 1b 01 00 09 00 2b 03 00 09 00 75 01 95 03 81 02");
    let f = &d.reports[0].fields[0];
    assert_eq!((f.usage(0), f.usage(2)), (Some(0x9_0001), Some(0x9_0003)));
}

#[test]
fn push_deeper_than_four() {
    // V1 refuses a fifth Push; V2 has no limit and Pop restores each level.
    let d = desc(
        "95 01 75 01 a4 75 02 a4 75 03 a4 75 04 a4 75 05 a4 75 06
         81 02 b4 81 02 b4 b4 b4 b4 81 02",
    );
    assert_eq!(d.max_push_depth, 5);
    let sizes: Vec<u32> = d.reports[0].fields.iter().map(|f| f.size).collect();
    assert_eq!(sizes, [6, 5, 1]);
    assert_eq!(d.reports[0].bits, 12);
}

#[test]
fn logical_maximum_sign_is_read_at_the_item() {
    // Maximum 0xFF read while the minimum is still 0: 255. The later Minimum -1 does not
    // turn it into -1 (V1 decided at the main item and saw -1..-1).
    let d = desc("15 00 25 ff 15 ff 75 08 95 01 81 02");
    let f = &d.reports[0].fields[0];
    assert_eq!((f.logical_min, f.logical_max), (-1, 255));
    assert_eq!(
        f.value(&[0xFF], 0),
        Ok(-1),
        "a negative minimum sign-extends the value"
    );
}
