//! Wave 43: the facts the power controller's start stands on: the answer codes, the PMFW's table by its
//! documented header, the link limits from configuration space, SCPM from the runtime database, the thermal
//! alert's registers.

use super::smuseq::{CAPS, table, table_bytes};
use crate::ipfacts::TABLE;
use crate::pcielink::{self, Host, LinkNow};
use crate::pptable14::{self, Bad, PcieLevels, Pptable};
use crate::regmap;
use crate::scpm::{self, Scpm};
use crate::smu14::{self, Resp};
use crate::thm14;

#[test]
fn an_answer_is_read_as_linux_reads_it() {
    // smu_cmn.c:249-292.
    for (answer, want, errno, hung, lets_send) in [
        (0, Resp::None, "-ETIME", false, false),
        (1, Resp::Ok, "0", false, true),
        (0xFF, Resp::Fail, "-EIO", false, true),
        (0xFE, Resp::Unknown, "-EOPNOTSUPP", false, true),
        (0xFD, Resp::BadPrereq, "-EINVAL", false, true),
        (0xFC, Resp::Busy, "-EBUSY", false, true),
        (0xFB, Resp::Other(0xFB), "-EREMOTEIO", true, false),
        (2, Resp::Other(2), "-EREMOTEIO", true, false),
        (u32::MAX, Resp::Other(u32::MAX), "-EREMOTEIO", true, false),
    ] {
        let r = smu14::decode(answer);
        assert_eq!((r, r.errno(), r.hung(), r.lets_send()), (want, errno, hung, lets_send), "{answer:#x}");
    }
    // usec_timeout (100000) * 20 reads (smu_v14_0_2_ppt.c:2154).
    assert_eq!(smu14::ANSWER_READS, 2_000_000);
    assert_eq!((smu14::freq_arg(9, 0xFF), smu14::freq_arg(2, 5)), (0x9_00FF, 0x2_0005));
    assert_eq!(smu14::DPM_FEATURES, 0x7A);
}

#[test]
fn the_table_is_read_at_linuxs_compiled_offsets() {
    let t = table();
    let bytes = table_bytes(&t);
    assert_eq!(pptable14::parse(&bytes), Ok(t));
    // Where the fields lie: `smu_14_0_2_powerplay_table` is packed, `PPTable_t` inside it is not. The whole
    // layout is held against Linux's headers by tests/pptable_offsets.rs; these are the bytes a table carries.
    assert_eq!(bytes[43..45], 118u16.to_le_bytes(), "software_shutdown_temp");
    assert_eq!((pptable14::SMC_PPTABLE, pptable14::SMC_PPTABLE % 32), (1344, 0), "on a 32-byte boundary, as the header asks");
    assert_eq!(bytes[1344 + 28 + 1108..][..6], [0, 2, 4, 6, 6, 6], "PcieGenSpeed, PcieLaneCount");
    assert_eq!(bytes[1344 + 28 + 2576 + 2..][..2], 2400u16.to_le_bytes(), "GameClockAc");
    assert_eq!(bytes[1344 + 4..][..4], 0x38FF_FCFBu32.to_le_bytes(), "FeaturesToRun[0]");
    assert!(pptable14::TABLE_BYTES <= smu14::DRIVER_TABLE_BYTES as usize && pptable14::SMC_PPTABLE + pptable14::PPTABLE_BYTES == pptable14::TABLE_BYTES);
    assert!(!t.no_fan() && Pptable { features_to_run: [0, 0], ..t }.no_fan());
    // What is no table for a message's argument.
    assert_eq!(pptable14::parse(&bytes[..pptable14::TABLE_BYTES - 1]), Err(Bad::Short { bytes: pptable14::TABLE_BYTES - 1 }));
    assert_eq!(pptable14::parse(&[0u8; 0x4000]), Err(Bad::Empty));
    let gen6 = table_bytes(&Pptable { pcie: PcieLevels { gen_speed: [0, 2, 5], ..t.pcie }, ..t });
    assert_eq!(pptable14::parse(&gen6), Err(Bad::PcieGen { level: 2, value: 5 }));
    let wide = table_bytes(&Pptable { pcie: PcieLevels { lane_count: [6, 7, 6], ..t.pcie }, ..t });
    assert_eq!(pptable14::parse(&wide), Err(Bad::PcieLane { level: 1, value: 7 }));
    // The table's own word of its parts is carried, not obeyed.
    let moved = Pptable { own: pptable14::OwnParts { pptable_start: 1376, ..t.own }, ..t };
    let m = pptable14::parse(&table_bytes(&moved)).unwrap();
    assert_eq!((m.own.pptable_start, m.pcie, m.game_clock_ac), (1376, t.pcie, 2400));
}

const X16: LinkNow = LinkNow { speed: 5, width: 16 };

#[test]
fn the_link_limit_is_linuxs_bandwidth_walk() {
    // The Gaming-PC on 18.11.: the card, its switch's two ports, the root port, all 32 GT/s x16.
    assert_eq!(pcielink::caps(&[X16; 4]), CAPS);
    assert_eq!(pcielink::limiting(&[X16; 4]), Some(X16));
    // The narrowest link wins, wherever it lies; of equals the one nearest the root (`next_bw <= bw`).
    let x8 = LinkNow { speed: 5, width: 8 };
    let gen4 = LinkNow { speed: 4, width: 16 };
    assert_eq!(pcielink::caps(&[X16, X16, X16, x8]), (4, 4));
    assert_eq!(pcielink::caps(&[X16, gen4, X16, X16]), (3, 6));
    // x16 at 16 GT/s carries a little less than x8 at 32 GT/s in Linux's integer arithmetic (252048 against 252056).
    assert_eq!((pcielink::limiting(&[x8, gen4, X16]), pcielink::limiting(&[gen4, x8, X16])), (Some(gen4), Some(gen4)));
    // Two links that carry exactly the same (x2 at 5 GT/s, x4 at 2.5 GT/s: 8000): the one nearer the root is taken.
    let (a, b) = (LinkNow { speed: 2, width: 2 }, LinkNow { speed: 1, width: 4 });
    assert_eq!((pcielink::limiting(&[X16, a, b]), pcielink::limiting(&[X16, b, a])), (Some(b), Some(a)));
    assert_eq!(pcielink::caps(&[X16, LinkNow { speed: 3, width: 4 }, x8]), (2, 3));
    // A bridge with no link reads 0 and counts 0: the next link replaces it (`!bw`); as the last it is the limit.
    let none = LinkNow { speed: 0, width: 0 };
    assert_eq!(pcielink::limiting(&[X16, none, x8]), Some(x8));
    assert_eq!(pcielink::caps(&[X16, x8, none]), (1, 0), "an unknown speed is Gen1 and Gen2, no width bit");
    // The codes (amdgpu_device.c:5966-5992, :6044-6100, amdgpu_smu.c:1815-1842).
    let cap = |speed, width| pcielink::caps(&[LinkNow { speed, width }; 2]);
    assert_eq!([cap(1, 1), cap(2, 2), cap(3, 4), cap(4, 8), cap(5, 12), cap(5, 16), cap(5, 32)], [(0, 1), (1, 2), (2, 3), (3, 4), (4, 5), (4, 6), (4, 7)]);
    assert_eq!(cap(6, 16), (0, 6), "64 GT/s falls into the source's last else: Gen1");
    assert_eq!(cap(5, 3), (4, 0), "a width that is none of Linux's sets no bit");
    // A card on the root bus: Linux's default masks.
    assert_eq!((pcielink::caps(&[X16]), pcielink::caps(&[])), ((1, 6), (1, 6)));
    assert_eq!((pcielink::mbs(5), pcielink::mbs(4), pcielink::mbs(1), pcielink::mbs(9)), (31507, 15753, 2000, 0));
}

#[test]
fn the_override_arguments_are_linuxs_forced_levels() {
    let t = table().pcie;
    // The trace's three (init.txt lines 952, 959, 966).
    assert_eq!(pcielink::forced(&t, 4, 6), ([0x0406, 0x1_0406, 0x2_0406], 3));
    // A narrower or slower platform: every level gets its limit.
    assert_eq!(pcielink::forced(&t, 4, 4), ([0x0404, 0x1_0404, 0x2_0404], 3));
    assert_eq!(pcielink::forced(&t, 3, 6), ([0x0306, 0x1_0306, 0x2_0306], 3));
    // A table whose last level is below the platform: the table's last level for all.
    let low = PcieLevels { gen_speed: [0, 1, 2], lane_count: [3, 3, 3], lclk_freq: [1, 2, 3] };
    assert_eq!(pcielink::forced(&low, 4, 6), ([0x0203, 0x1_0203, 0x2_0203], 3));
    // Levels that are all zero are left out, and the levels are numbered as they remain.
    let two = PcieLevels { gen_speed: [0, 0, 4], lane_count: [0, 6, 6], lclk_freq: [0, 0, 619] };
    assert_eq!(pcielink::forced(&two, 4, 6), ([0x0406, 0x1_0406, 0], 2));
    assert_eq!(pcielink::forced(&PcieLevels::default(), 4, 6).1, 0);
}

#[test]
fn linux_leaves_the_links_speed_alone_on_intel_hosts() {
    let name = |s: &[u8; 12]| [u32::from_le_bytes([s[0], s[1], s[2], s[3]]), u32::from_le_bytes([s[4], s[5], s[6], s[7]]), u32::from_le_bytes([s[8], s[9], s[10], s[11]])];
    // The Gaming-PC's Core Ultra 7 265K (family 6, model 0xC6), and any Intel family.
    assert_eq!(pcielink::host(name(b"GenuineIntel"), 0x000C_0662), Host::Fixed);
    assert_eq!(pcielink::host(name(b"GenuineIntel"), 0x0000_0F41), Host::Fixed);
    // AMD: only Zen+ (family 0x17 model 0x08); the Mini-PC's Ryzen (family 0x19) is not on the list.
    assert_eq!(pcielink::host(name(b"AuthenticAMD"), 0x0080_0F82), Host::Fixed);
    assert_eq!(pcielink::host(name(b"AuthenticAMD"), 0x0080_0F12), Host::Switching, "family 0x17 model 0x01");
    assert_eq!(pcielink::host(name(b"AuthenticAMD"), 0x00A7_0F52), Host::Switching);
    assert_eq!(pcielink::host([0; 3], 0), Host::Switching);
}

#[test]
fn scpm_is_read_from_the_runtime_database_as_linux_reads_it() {
    let db = scpm::synthetic(1);
    let read = |db: &[u8]| -> Scpm {
        let h: [u8; 4] = db[..4].try_into().unwrap();
        if let Err(no) = scpm::header(&h) {
            return no;
        }
        let mut dir = [0u8; scpm::DIRECTORY_BYTES];
        let n = (db.len() - 4).min(dir.len());
        dir[..n].copy_from_slice(&db[4..4 + n]);
        match scpm::entry(&dir) {
            Ok(at) => scpm::status(&db[usize::from(at)..][..4].try_into().unwrap()),
            Err(no) => no,
        }
    };
    assert_eq!((read(&db), read(&scpm::synthetic(2)), read(&scpm::synthetic(0))), (Scpm::Enabled { status: 1 }, Scpm::Enabled { status: 2 }, Scpm::Disabled));
    assert!(Scpm::Enabled { status: 2 }.enabled() && !Scpm::Disabled.enabled() && !Scpm::NoEntry.enabled() && !Scpm::Unread.enabled());
    // No cookie: no database (a card that reads zeros there).
    assert_eq!(read(&[0u8; 20]), Scpm::NoDatabase { cookie: 0 });
    let mut b = db;
    b[0] = 0xD4;
    assert_eq!(read(&b), Scpm::NoDatabase { cookie: 0x0ED4 });
    // A count Linux refuses, an entry of another type, an entry smaller than its struct.
    let mut b = db;
    b[4..6].copy_from_slice(&0x40u16.to_le_bytes());
    assert_eq!(read(&b), Scpm::BadCount { count: 0x40 });
    let mut b = db;
    b[8] = 5;
    assert_eq!(read(&b), Scpm::NoEntry);
    let mut b = db;
    b[4] = 0;
    assert_eq!(read(&b), Scpm::NoEntry, "no entries at all");
    let mut b = db;
    b[14] = 3;
    assert_eq!(read(&b), Scpm::BadSize { size: 3 });
    assert_eq!((scpm::FROM_END, scpm::DIRECTORY_BYTES, scpm::COOKIE), (1 << 20, 516, 0x0ED5));
}

#[test]
fn the_thermal_alerts_registers_are_linuxs_changes_of_what_the_card_read() {
    // The trace: init.txt lines 1870-1876.
    assert_eq!(thm14::int_ctrl(0x2FFF_0071, 118, smu14::THERMAL_MIN_MILLI), 0xB8FF_0076);
    assert_eq!((thm14::INT_ENA, thm14::sw_int(0), thm14::sw_int_ctrl(0)), (0x38, 0xFE, 0));
    // The upper threshold is at most 255 degrees, the lower at least 0; the other fields stay as read.
    assert_eq!(thm14::int_ctrl(0x2FFF_0071, 300, smu14::THERMAL_MIN_MILLI) & 0xFFFF, 0x00FF);
    assert_eq!(thm14::int_ctrl(0x2FFF_AB71, 118, 20_000) & 0xFFFF, 0x1476);
    assert_eq!(thm14::int_ctrl(0, 118, smu14::THERMAL_MIN_MILLI), 0xB000_0076);
    assert_eq!(thm14::int_ctrl(u32::MAX, 118, smu14::THERMAL_MIN_MILLI), 0xB8FF_0076);
    assert_eq!((thm14::sw_int(0xFFFF_FFFF), thm14::sw_int_ctrl(0x101)), (0xFFFF_FEFE, 0x100));
    // Where they lie on this card: the trace's dwords.
    let at = |r| regmap::dword(&TABLE, r);
    assert_eq!([at(thm14::THERMAL_INT_ENA), at(thm14::THERMAL_INT_CTRL), at(thm14::MP1_IH_SW_INT), at(thm14::MP1_IH_SW_INT_CTRL)], [Some(0x1660A), Some(0x1660B), Some(0x16341), Some(0x16342)]);
}
