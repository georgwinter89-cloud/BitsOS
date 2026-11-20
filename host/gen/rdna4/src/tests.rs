use crate::emit::{BLOCKS, Block};
use crate::parse::{self, Kind};

const OFFSET: &str = "#ifndef _mini_OFFSET_HEADER\n#define _mini_OFFSET_HEADER\n// addressBlock: a\n#define regFOO   0x0080\n#define regFOO_BASE_IDX   1\n#define mmBAR 0x12\n#define mmBAR_BASE_IDX 0\n#define ixIDX 0x7\n#define cfgVENDOR_ID 0x0000\n#endif\n";
const MASK: &str = "#define _mini_SH_MASK_HEADER\n//FOO\n#define FOO__A__SHIFT 0x0\n#define FOO__B__X__SHIFT 0x4\n#define FOO__A_MASK 0x0000000FL\n#define FOO__B__X_MASK 0x000000F0L\n#define NOBODY__Z__SHIFT 0x0\n#define NOBODY__Z_MASK 0x1L\n";

#[test]
fn a_block_is_parsed_and_rendered() {
    let mut regs = parse::offsets(OFFSET, &[], &[]).unwrap();
    assert_eq!(regs.iter().map(|r| (r.name.as_str(), r.kind, r.offset, r.base_idx)).collect::<Vec<_>>(), [
        ("FOO", Kind::Mmio, 0x80, Some(1)),
        ("BAR", Kind::Mmio, 0x12, Some(0)),
        ("IDX", Kind::Indirect, 7, None),
        ("VENDOR_ID", Kind::Config, 0, None),
    ]);
    let (orphans, halves) = parse::fields(MASK, &mut regs, &[]).unwrap();
    assert_eq!(orphans, ["NOBODY__Z__SHIFT", "NOBODY__Z_MASK"]);
    assert!(halves.is_empty());
    assert_eq!(regs[0].fields.iter().map(|f| (f.name.as_str(), f.shift, f.mask)).collect::<Vec<_>>(), [("A", 0, 0xF), ("B__X", 4, 0xF0)]);
    let (text, c) = crate::generate(&Block { name: "mini", dir: "mini", headerless: &[], wide: &[], stray: &[] }, OFFSET, MASK).unwrap();
    assert_eq!((c.mmio, c.indirect, c.config, c.fields), (2, 1, 1, 2));
    for line in [
        "use crate::{Cfg, Field, Ix, Reg};",
        "pub const FOO: Reg = Reg::new(\"FOO\", 1, 0x0080);",
        "pub static REGS: [Reg; 2] = [",
        "    pub const IDX: Ix = Ix::new(\"IDX\", 0x0007);",
        "    pub const VENDOR_ID: Cfg = Cfg::new(\"VENDOR_ID\", 0x0000);",
        "    pub const FOO__B__X: Field = Field::new(4, 0x000000f0);",
        "// 2 mask defines name no register",
    ] {
        assert!(text.contains(line), "{line}\n{text}");
    }
}

#[test]
fn what_is_not_understood_is_refused() {
    let no = |text: &str, part: &str| {
        let e = parse::offsets(text, &[], &[]).unwrap_err();
        assert!(e.contains(part), "{e}");
    };
    no("#define regA 0x1\n", "no BASE_IDX");
    no("#define regA 0x1\n#define regA_BASE_IDX 0\n#define regA 0x2\n", "twice");
    no("#define regA 0x1 0x2\n", "unexpected define");
    no("#define fooA 0x1\n", "unknown prefix");
    no("#define regA 0x1\n#define regA_BASE_IDX 9\n", "BASE_IDX");
    assert_eq!(parse::offsets("#define regA 0x1\n#define regB 0x2\n#define regB_BASE_IDX 1\n", &["A"], &[]).unwrap().len(), 1, "a listed one is left out");
    assert!(parse::offsets("#define regA 0x1\n#define regA_BASE_IDX 0\n", &["A"], &[]).unwrap_err().contains("correct the list"));
    assert_eq!(parse::offsets("#define regB_BASE_IDX 1\n", &[], &["regB_BASE_IDX"]).unwrap().len(), 0, "a stray define is skipped");
    assert!(parse::offsets("", &[], &["regB_BASE_IDX"]).unwrap_err().contains("stray"));
    let mut regs = parse::offsets("#define regA 0x1\n#define regA_BASE_IDX 0\n", &[], &[]).unwrap();
    assert_eq!(parse::fields("#define A__F__SHIFT 0x0\n", &mut regs.clone(), &[]).unwrap().1, ["A__F"], "half a field is left out and named");
    assert!(parse::fields("#define A__F__SHIFT 0x4\n#define A__F_MASK 0x3L\n", &mut regs.clone(), &[]).unwrap_err().contains("does not fit"));
    assert!(parse::fields("#define A__F 0x4\n", &mut regs.clone(), &[]).unwrap_err().contains("neither"));
    let wide = "#define A__F__SHIFT 0x8\n#define A__F_MASK 0xFFFFFFFFFFFFFF00L\n";
    assert!(parse::fields(wide, &mut regs.clone(), &[]).unwrap_err().contains("32 bits"));
    assert!(parse::fields(wide, &mut regs, &["A"]).unwrap().0.is_empty());
    assert!(regs[0].fields.is_empty(), "a listed 64-bit register keeps no field");
}


#[test]
fn the_block_list_is_item_5s() {
    assert_eq!(BLOCKS.iter().map(|b| b.name).collect::<Vec<_>>(), [
        "gc_12_0_0", "mp_14_0_2", "nbif_6_3_1", "osssys_7_0_0", "mmhub_4_1_0", "athub_4_1_0", "hdp_7_0_0", "lsdma_7_0_0", "smuio_14_0_2", "thm_14_0_2"
    ]);
}
