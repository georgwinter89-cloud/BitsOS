//! KMS view of the saved-5/G-1 image, the BO manager, the IP table, the tools' text.

use super::fake::*;
use crate::bo::{self, BoError, BoManager, Placement, Reserved};
use crate::ip::{self, IpKind, Source};
use crate::kms::{self, ConnectorKind, EncoderMode};
use crate::layout as l;
use crate::{attach, dcn, tools};
use alloc::string::String;
use alloc::vec;

#[test]
fn kms_topology_of_the_mini_pc() {
    let t = kms::read(&reader(&mini_pc()), true);
    let c0 = t.crtcs[0];
    assert!(c0.enabled && t.crtcs[1..].iter().all(|c| !c.enabled));
    assert_eq!((c0.h_active, c0.v_active, c0.h_total, c0.v_total), (2560, 1440, 2720, 1481));
    assert_eq!((c0.pixel_hz, c0.refresh_mhz), (Some(241_500_000), Some(59950)));
    let vis: alloc::vec::Vec<_> = t.planes.iter().filter(|p| p.visible).collect();
    assert_eq!(vis.len(), 1);
    assert_eq!(
        (vis[0].id, vis[0].crtc, vis[0].width, vis[0].height, vis[0].pitch, vis[0].format, vis[0].address),
        (0, 0, 1024, 768, 1024, 8, GPU)
    );
    assert_eq!(t.encoders, vec![kms::Encoder { id: 0, crtc: Some(0), mode: EncoderMode::DpSst }]);
    assert_eq!(
        t.connectors,
        vec![kms::Connector {
            id: 0,
            encoder: 0,
            kind: ConnectorKind::DisplayPort,
            link_trained: true,
            stream_on: true
        }]
    );
    let lines = t.lines();
    assert!(
        lines[0].starts_with("crtc0 (OTG0): 2560x1440 of 2720x1481, pixel clock 241500000 Hz, 59.950 Hz"),
        "{lines:?}"
    );
    // Narrow: the same objects (OTG0 drives the one picture, DIG0 is the DP), fewer instances read.
    let n = kms::read(&reader(&mini_pc()), false);
    assert_eq!((n.planes, n.encoders, n.connectors), (t.planes.clone(), t.encoders.clone(), t.connectors.clone()));
    let ids: alloc::vec::Vec<u8> = n.crtcs.iter().map(|c| c.id).collect();
    assert!(ids.contains(&0) && ids.len() < t.crtcs.len(), "{ids:?}");
}

#[test]
fn kms_skips_gated_pipes_and_off_encoders() {
    let mut img = mini_pc();
    img.insert(l::pg_status(0), 0x4000_0000);
    img.insert(l::dig(0, l::DIG_BE_EN_CNTL), 0);
    let t = kms::read(&reader(&img), true);
    assert!(t.planes.iter().all(|p| p.id != 0), "pipe 0 needs domain 0");
    assert!(t.encoders.is_empty() && t.connectors.is_empty());
}

fn manager() -> (attach::Attach, BoManager) {
    let a = attach::gates(&pci(), Some(gop()), &reader(&mini_pc())).unwrap();
    let m = BoManager::new(a.fb_last - a.fb_first + 1, bo::reserved_from(&a));
    (a, m)
}

#[test]
fn reserved_ranges_of_the_carve_out() {
    let (a, m) = manager();
    let r = m.reserved();
    assert_eq!(m.size(), 8 << 30);
    assert_eq!(r[0], Reserved { name: String::from("gop"), range: 0..BYTES });
    assert_eq!(r[1], Reserved { name: String::from("dmcub REGION3_CW0"), range: 0x1_FFE0_0000..0x1_FFF0_0000 });
    assert_eq!(r[2], Reserved { name: String::from("discovery"), range: (8 << 30) - (64 << 10)..8 << 30 });
    assert_eq!(m.free_bytes(), (8 << 30) - BYTES - (1 << 20) - (64 << 10));
    assert_eq!(a.a1 - a.fb_first, 32 * MB);
}

#[test]
fn bo_hostile_sizes_and_offsets() {
    let (_, mut m) = manager();
    let free = m.free_bytes();
    // An end near u64::MAX: refused before the page rounding could overflow.
    for end in [u64::MAX, u64::MAX - 1, u64::MAX - 4095, (8u64 << 30) + 1] {
        assert_eq!(m.create_at(64 * MB..end), Err(BoError::Busy), "end {end:#x}");
    }
    assert_eq!(m.create(u64::MAX, Placement::Vram, 4096, None), Err(BoError::NoSpace));
    assert_eq!(m.create(u64::MAX - 4095, Placement::Vram, 1 << 63, None), Err(BoError::NoSpace));
    assert_eq!(m.free_bytes(), free);
    // The mmap offset space used up: refused, and the range is not taken.
    m.set_next_mmap(u64::MAX - 4096);
    assert_eq!(m.create(8192, Placement::Vram, 4096, None), Err(BoError::NoMmapSpace));
    assert_eq!(m.create_at(64 * MB..65 * MB), Err(BoError::NoMmapSpace));
    assert_eq!(m.free_bytes(), free);
    let h = m.create(4096, Placement::Vram, 4096, None).unwrap();
    assert_eq!(m.by_mmap(u64::MAX - 1).unwrap().handle, h);
    assert!(m.by_mmap(u64::MAX).is_none());
    assert_eq!(m.create(4096, Placement::Vram, 4096, None), Err(BoError::NoMmapSpace));
}

#[test]
fn bo_lifecycle() {
    let (a, mut m) = manager();
    let gop = m.adopt("gop").unwrap();
    assert_eq!(m.get(gop).unwrap().range, 0..BYTES);
    assert_eq!(m.adopt("gop"), Err(BoError::Busy));
    // GX-3's back buffer where the gates checked it.
    let back = m.create_at(a.a1 - a.fb_first..a.a1 - a.fb_first + BYTES).unwrap();
    assert_eq!(m.create_at(a.a1 - a.fb_first..a.a1 - a.fb_first + 4096), Err(BoError::Busy));
    assert_eq!(m.create_at(0..4096), Err(BoError::Busy), "inside the GOP framebuffer");
    // A CPU-visible BO below 256 MB, 2 MB aligned: first fit after the GOP framebuffer.
    let x = m.create(5 * MB, Placement::Vram, 2 * MB, Some(256 * MB)).unwrap();
    assert_eq!(m.get(x).unwrap().range, 4 * MB..9 * MB);
    assert_eq!(m.create(300 * MB, Placement::Vram, 4096, Some(256 * MB)), Err(BoError::NoSpace));
    assert_eq!(m.create(0, Placement::Vram, 4096, None), Err(BoError::ZeroSize));
    assert_eq!(m.create(4096, Placement::Vram, 3 * 4096, None), Err(BoError::BadAlign));
    assert_eq!(m.create(4096, Placement::System, 4096, None), Err(BoError::PlacementUnsupported));
    // mmap offsets: distinct, page aligned, found again.
    let (o1, o2) = (m.get(back).unwrap().mmap_offset, m.get(x).unwrap().mmap_offset);
    assert!(o1 >= bo::MMAP_BASE && o1 % bo::PAGE == 0 && o2 >= o1 + BYTES);
    assert_eq!(m.by_mmap(o2 + 4096).unwrap().handle, x);
    // Reference counts; the freed range coalesces back.
    let free = m.free_bytes();
    assert_eq!(m.reference(x), Some(2));
    assert_eq!(m.release(x), Some(1));
    assert_eq!(m.release(x), Some(0));
    assert!(m.get(x).is_none());
    assert_eq!(m.free_bytes(), free + 5 * MB);
    assert_eq!(
        m.create(5 * MB, Placement::Vram, 2 * MB, Some(256 * MB)).map(|h| m.get(h).unwrap().range.clone()),
        Ok(4 * MB..9 * MB)
    );
    // Releasing the adopted GOP BO does not hand the firmware's framebuffer to the allocator.
    let before = m.free_bytes();
    m.release(gop);
    assert_eq!(m.free_bytes(), before);
    assert_eq!(m.release(9999), None);
}

#[test]
fn ip_blocks() {
    let mut b = ip::known_blocks(0x15BF);
    assert_eq!(b, ip::known_blocks(0x1900));
    assert_eq!(ip::block(&b, IpKind::Gc).unwrap().version, ip::IpVersion { major: 11, minor: 0, rev: 1 });
    assert!(matches!(ip::block(&b, IpKind::Dcn).unwrap().source, Source::Table(_)));
    ip::mark_dcn_measured(&mut b);
    assert!(matches!(ip::block(&b, IpKind::Dcn).unwrap().source, Source::Measured(_)));
    let mut unknown = ip::known_blocks(0x7550);
    assert!(unknown.is_empty());
    ip::mark_dcn_measured(&mut unknown);
    assert_eq!(alloc::format!("{}", unknown[0].version), "3.1.4");
}

#[test]
fn writes_text() {
    let w = [
        dcn::LoggedWrite { seq: 7, offset: 0xEB2C, wrote: 0x80, read: 0x80, masked: false },
        dcn::LoggedWrite { seq: 8, offset: 0xEB28, wrote: 0x200_0000, read: 0, masked: false },
        dcn::LoggedWrite { seq: 9, offset: 0x14128, wrote: 1, read: 0x101, masked: true },
    ];
    let s = tools::writes_text(9, 1, &w);
    assert_eq!(
        s,
        "writes 9, read back different 1, last 3 shown\n#7 0EB2C HUBPREQ0_DCSURF_PRIMARY_SURFACE_ADDRESS_HIGH wrote 0x00000080 read 0x00000080\n#8 0EB28 HUBPREQ0_DCSURF_PRIMARY_SURFACE_ADDRESS wrote 0x02000000 read 0x00000000 MISMATCH\n#9 14128 OTG0_OTG_MASTER_UPDATE_LOCK wrote 0x00000001 read 0x00000101 (status bits)\n"
    );
}
