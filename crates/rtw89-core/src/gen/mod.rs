//! Tables and register names generated from the JS driver (`gen/emit.mjs`), and
//! the row types they use. Only this file is written by hand; the others are
//! regenerated and must not be edited, and are not formatted (`emit.mjs --check`
//! compares them byte for byte).

#[rustfmt::skip]
pub mod btc;
#[rustfmt::skip]
pub mod chan;
#[rustfmt::skip]
pub mod dmac;
#[rustfmt::skip]
pub mod fw_feat;
#[rustfmt::skip]
pub mod phy;
#[rustfmt::skip]
pub mod regs;
#[rustfmt::skip]
pub mod rfk1;
#[rustfmt::skip]
pub mod rfk2;
#[rustfmt::skip]
pub mod trk;
#[rustfmt::skip]
pub mod txrx;

/// The value of a register or enum name, for logs and tools (`None`: not one the JS driver uses).
pub fn reg_value(name: &str) -> Option<i64> {
    regs::NAMES
        .binary_search_by(|(n, _)| (*n).cmp(name))
        .ok()
        .map(|i| regs::NAMES[i].1)
}

/// struct rtw89_dle_size
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DleSize {
    pub pge_size: u16,
    pub lnk_pge_num: u16,
    pub unlnk_pge_num: u16,
}

/// struct rtw89_dle_mem: one quota mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DleMem {
    pub mode: &'static str,
    pub wde_size: DleSize,
    pub ple_size: DleSize,
    pub wde_min: &'static [u16],
    pub wde_max: &'static [u16],
    pub ple_min: &'static [u16],
    pub ple_max: &'static [u16],
}

/// struct rtw89_hfc_ch_cfg
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HfcCh {
    pub min: u16,
    pub max: u16,
    pub grp: u16,
}

/// struct rtw89_hfc_pub_cfg
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HfcPub {
    pub grp0: u16,
    pub grp1: u16,
    pub pub_max: u16,
    pub wp_thrd: u16,
}

/// struct rtw89_hfc_prec_cfg
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HfcPrec {
    pub ch011_prec: u16,
    pub h2c_prec: u16,
    pub wp_ch07_prec: u16,
    pub wp_ch811_prec: u16,
    pub ch011_full: u16,
    pub h2c_full: u16,
    pub wp_ch07_full: u16,
    pub wp_ch811_full: u16,
}

/// How a fw_feat_tbl row compares the firmware's version code with its own
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatCond {
    Ge,
    Le,
    Lt,
}

/// A row of fw_feat_tbl (8852B)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FwFeat {
    pub cond: FeatCond,
    pub code: u32,
    pub feature: u32,
    pub name: &'static str,
}

/// A coex TDMA slot (struct rtw89_btc_fbtc_slot)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BtcSlot {
    pub dur: u16,
    pub cxtbl: u32,
    pub cxtype: u16,
}

/// A coex monitor register (struct rtw89_btc_fbtc_mreg)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonReg {
    pub kind: u16,
    pub bytes: u16,
    pub offset: u32,
}

/// A channel range of a country's rules
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChanRule {
    pub first: u8,
    pub last: u8,
    pub no_ir: bool,
    pub radar: bool,
}

/// A BB gain register: 2 GHz address of paths A and B, 5 GHz ones, the field
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GainReg {
    pub g2: [u16; 2],
    pub g5: [u16; 2],
    pub mask: u32,
}

/// A TX FIR coefficient register with its values on channel 14 and elsewhere
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxFir {
    pub reg: u32,
    pub mask: u32,
    pub ch14: u32,
    pub other: u32,
}

/// A TX power firmware element and the configuration it fills
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxpwrConf {
    pub id: u8,
    pub key: &'static str,
}

/// rtw89_phy_write32_mask(addr, mask, data)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaskWrite {
    pub addr: u32,
    pub mask: u32,
    pub data: u32,
}

/// One entry of an RFK sequence table (RTW89_DECL_RFK_*)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rfk {
    /// BB write32_mask
    Wm { addr: u32, mask: u32, data: u32 },
    /// BB write32_set
    Ws { addr: u32, bits: u32 },
    /// BB write32_clr
    Wc { addr: u32, bits: u32 },
    /// udelay
    Delay { us: u32 },
    /// RF write on path 0 (A) or 1 (B)
    Wrf {
        path: u8,
        addr: u32,
        mask: u32,
        data: u32,
    },
}

/// A channel range -> TSSI group (`extra`: the channel lies between two groups)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TssiGroup {
    pub first: u8,
    pub last: u8,
    pub group: u8,
    pub extra: bool,
}
