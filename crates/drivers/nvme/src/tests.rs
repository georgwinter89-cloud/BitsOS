//! Host tests: the command builder, rings, PRPs, Identify parsing, and the controller logic against the fake.

use crate::cmd::{self, AdminCmd, Encode, IoCmd, MEDIA_WRITE_ADMIN_OPS, MEDIA_WRITE_IO_OPS};
use crate::ctrl::{CmdError, Config, Controller, State};
use crate::fake::{self, FakeHw};
use crate::hw::{DmaMem, IrqMode, VecMem};
use crate::ns::NsDevice;
use crate::queue::QueuePair;
use crate::{ident, prp, regs};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use devmodel::DevError;
use devmodel::block::{BlockDevice, IoPriority};

fn ctrl(nss: Vec<fake::FakeNs>) -> (Arc<Controller<FakeHw>>, FakeHw) {
    let hw = FakeHw::new(nss);
    let c = Arc::new(Controller::new(String::from("nvme9"), hw.clone(), Config::default()));
    (c, hw)
}

fn up(nss: Vec<fake::FakeNs>) -> (Arc<Controller<FakeHw>>, FakeHw) {
    let (c, hw) = ctrl(nss);
    c.start().unwrap();
    assert_eq!(c.state(), State::Ready);
    (c, hw)
}

fn no_writes(c: &Controller<FakeHw>, hw: &FakeHw) {
    let st = hw.st.lock();
    for op in &st.seen_io {
        assert!(!MEDIA_WRITE_IO_OPS.contains(op), "I/O opcode {op:#x} reached the device");
    }
    for op in &st.seen_admin {
        assert!(!MEDIA_WRITE_ADMIN_OPS.contains(op), "admin opcode {op:#x} reached the device");
    }
    assert_eq!(c.stats.media_writes(), 0);
}

#[test]
fn builder_has_no_media_writing_opcode() {
    for op in cmd::IO_OPS {
        assert!(!MEDIA_WRITE_IO_OPS.contains(&(op as u8)), "{op:?}");
    }
    for op in cmd::ADMIN_OPS {
        assert!(!MEDIA_WRITE_ADMIN_OPS.contains(&(op as u8)), "{op:?}");
    }
    // Every constructor, encoded.
    let io = [IoCmd::read(1, 5, 8, 0x1000, 0), IoCmd::flush(1)];
    for c in io {
        let b = c.encode(7);
        assert!(b[0] == 0x00 || b[0] == 0x02);
        assert_eq!(u16::from_le_bytes([b[2], b[3]]), 7);
    }
    let r = IoCmd::read(3, 0x1_2345_6789, 65536, 0xA000, 0xB000).encode(1);
    assert_eq!(u32::from_le_bytes(r[4..8].try_into().unwrap()), 3);
    assert_eq!(u64::from_le_bytes(r[24..32].try_into().unwrap()), 0xA000);
    assert_eq!(u64::from_le_bytes(r[32..40].try_into().unwrap()), 0xB000);
    assert_eq!(u32::from_le_bytes(r[40..44].try_into().unwrap()), 0x2345_6789);
    assert_eq!(u32::from_le_bytes(r[44..48].try_into().unwrap()), 1);
    assert_eq!(u32::from_le_bytes(r[48..52].try_into().unwrap()), 0xFFFF);
    let set = AdminCmd::set_num_queues(1, 1).encode(0);
    assert_eq!(set[0], 0x09);
    assert_eq!(set[40], cmd::FEAT_NUM_QUEUES);
}

#[test]
fn cqe_status_decoding() {
    let c = cmd::Cqe::from_dwords([5, 0, 0x0001_0003, 0x2 << 17 | 1 << 16 | 9 | 1 << 31]);
    assert_eq!((c.dw0, c.sq_head, c.sq_id, c.cid), (5, 3, 1, 9));
    assert!(!c.ok());
    assert_eq!(c.sc(), 2);
    assert!(c.dnr());
    assert!(cmd::status_is_invalid(c.sct(), c.sc()));
    let m = cmd::Cqe::from_dwords([0, 0, 0, 0x281 << 17]);
    assert_eq!((m.sct(), m.sc()), (2, 0x81));
    assert!(!cmd::status_is_invalid(m.sct(), m.sc()));
}

fn mem(addr: u64, n: usize) -> VecMem {
    VecMem { addr, bytes: Arc::new(ksync::SpinLock::new(vec![0xEE; n])) }
}

#[test]
fn ring_phase_and_cid_wrap() {
    let mut q: QueuePair<VecMem, IoCmd> = QueuePair::new(1, 4, mem(0x1000, 256), mem(0x2000, 64)).unwrap();
    assert!(QueuePair::<VecMem, IoCmd>::new(1, 4, mem(0, 100), mem(0, 64)).is_none());
    let mut posted_phase = true;
    let mut tail = 0usize;
    for round in 0..10u32 {
        let (cid, t, op) = q.push(&IoCmd::flush(1)).unwrap();
        assert_eq!(op, 0);
        assert_eq!(t as u32, (round + 1) % 4);
        // The controller posts a completion at CQ `tail` with the current phase.
        let d3 = cid as u32 | (posted_phase as u32) << 16;
        assert!(q.cq.copy_in(tail * 16 + 12, &d3.to_le_bytes()));
        assert!(q.cq.copy_in(tail * 16 + 8, &(t as u32 | 1 << 16).to_le_bytes()));
        let (c, head) = q.pop().unwrap().unwrap();
        assert_eq!(c.cid, cid);
        assert_eq!(head as usize, (tail + 1) % 4);
        assert!(q.pop().unwrap().is_none(), "old phase must not count");
        tail = (tail + 1) % 4;
        if tail == 0 {
            posted_phase = !posted_phase;
        }
    }
    // Full: three pushes without the controller moving its head.
    let mut q: QueuePair<VecMem, IoCmd> = QueuePair::new(1, 4, mem(0x1000, 256), mem(0x2000, 64)).unwrap();
    for _ in 0..3 {
        q.push(&IoCmd::flush(1)).unwrap();
    }
    assert!(q.push(&IoCmd::flush(1)).is_err());
}

#[test]
fn prp_cases() {
    let p = regs::PAGE as u64;
    assert_eq!(prp::plan(0x10000, 512, 0x9000).unwrap(), prp::Prps { prp1: 0x10000, prp2: 0, list_len: 0 });
    assert_eq!(prp::plan(0x10000, 8192, 0x9000).unwrap(), prp::Prps { prp1: 0x10000, prp2: 0x10000 + p, list_len: 0 });
    assert_eq!(prp::plan(0x10000, 8193, 0x9000).unwrap(), prp::Prps { prp1: 0x10000, prp2: 0x9000, list_len: 2 });
    assert_eq!(prp::list_entry(0x10000, 1), 0x10000 + 2 * p);
    assert!(prp::plan(0x10001, 512, 0).is_none());
    assert!(prp::plan(0x10000, 0, 0).is_none());
    assert!(prp::plan(0x10000, prp::max_bytes() + 1, 0).is_none());
    assert_eq!(prp::plan(0x10000, prp::max_bytes(), 0x9000).unwrap().list_len, regs::PAGE / 8);
}

#[test]
fn identify_parsing_is_bounded() {
    let mut p = vec![0u8; 4096];
    p[4..12].copy_from_slice(b"SN123   ");
    p[24..29].copy_from_slice(b"Model");
    p[30] = 0xFF;
    p[77] = 7;
    p[516] = 3;
    p[525] = 1;
    let id = ident::parse_ctrl(&p);
    assert_eq!(id.serial, "SN123");
    assert_eq!(id.model, "Model ?");
    assert_eq!(id.max_transfer(12), Some(512 * 1024));
    assert_eq!(id.nn, 3);
    assert!(id.volatile_cache());
    // A short page never panics.
    let short = ident::parse_ctrl(&p[..10]);
    assert_eq!(short.nn, 0);
    let ns = ident::parse_ns(&[0xFF; 30]);
    assert!(ns.unusable().is_some());
    let mut list = vec![0u8; 32];
    for (i, v) in [5u32, 3, 9, 0xFFFF_FFFF, 12, 0, 77].iter().enumerate() {
        list[4 * i..4 * i + 4].copy_from_slice(&v.to_le_bytes());
    }
    assert_eq!(ident::parse_ns_list(&list, 0), vec![5, 9, 12]);
    // More than 16 formats: FLBAS bits 6:5 are the upper bits.
    let mut n = vec![0u8; 4096];
    n[0] = 8;
    n[25] = 20;
    n[26] = 0x21;
    n[128 + 4 * 17 + 2] = 12;
    let id = ident::parse_ns(&n);
    assert_eq!(id.format_index(), 17);
    assert_eq!(id.block_size(), 4096);
    assert!(id.unusable().is_none());
}

#[test]
fn bring_up_identifies_and_reads_both_formats() {
    let (c, hw) = up(vec![fake::ns(1, 9, 2048), fake::ns(2, 12, 256)]);
    let i = c.info();
    assert_eq!(i.namespaces.len(), 2);
    assert_eq!(i.namespaces[0].block_size(), 512);
    assert_eq!(i.namespaces[1].block_size(), 4096);
    assert_eq!(i.ident.as_ref().unwrap().serial, "FAKE0001");
    assert_eq!(i.max_transfer, 128 * 1024);
    assert_eq!(i.irq, Some(IrqMode::MsiX(2)));
    assert_eq!(i.io_vector, Some(1));
    for (nsid, bs) in [(1u32, 512usize), (2, 4096)] {
        // One block, two pages, a PRP list, more than one transfer.
        for (lba, len) in [(0u64, bs), (3, 8192), (1, 64 * 1024), (5, 300 * 1024)] {
            let mut b = vec![0u8; len];
            c.read(nsid, lba, &mut b, true).unwrap();
            let base = lba * bs as u64;
            assert!(b.iter().enumerate().all(|(k, &x)| x == fake::pattern(nsid, base + k as u64)), "ns{nsid} lba {lba} len {len}");
        }
    }
    assert_eq!(c.read(1, 2047, &mut [0u8; 1024], true), Err(CmdError::Invalid));
    assert_eq!(c.read(1, 0, &mut [0u8; 100], true), Err(CmdError::Invalid));
    assert_eq!(c.read(7, 0, &mut [0u8; 512], true), Err(CmdError::Invalid));
    c.flush(1).unwrap();
    let s = c.smart().unwrap();
    assert_eq!((s.temperature_k, s.available_spare, s.power_cycles, s.power_on_hours, s.unsafe_shutdowns), (310, 100, 7, 42, 3));
    no_writes(&c, &hw);
}

#[test]
fn namespace_device_is_read_only() {
    let (c, hw) = up(vec![fake::ns(1, 9, 1024)]);
    let d = NsDevice::new(c.clone(), 1).unwrap();
    assert_eq!(d.name(), "nvme9n1");
    let info = d.info();
    assert!(info.read_only);
    assert!(!info.supports_discard);
    assert!(info.has_volatile_cache);
    assert_eq!(info.max_blocks_per_request, 256);
    let mut b = vec![0u8; 1024];
    d.read_blocks(10, &mut b, IoPriority::Normal).unwrap();
    assert_eq!(b[0], fake::pattern(1, 10 * 512));
    assert_eq!(d.write_blocks(0, &b, true, IoPriority::Normal), Err(DevError::Refused));
    assert_eq!(d.discard(0, 8), Err(DevError::Refused));
    assert_eq!(d.read_blocks(1023, &mut b, IoPriority::Normal), Err(DevError::InvalidArgument));
    d.flush().unwrap();
    let before = hw.st.lock().seen_io.len();
    let _ = d.write_blocks(5, &b, false, IoPriority::Normal);
    assert_eq!(hw.st.lock().seen_io.len(), before, "a refused write must not reach the queue");
    no_writes(&c, &hw);
    assert!(NsDevice::new(c, 2).is_none());
}

#[test]
fn lost_completion_resets_and_retries_once() {
    let (c, hw) = up(vec![fake::ns(1, 9, 1024)]);
    hw.st.lock().inject.drop_io = 1;
    let mut b = vec![0u8; 4096];
    c.read(1, 8, &mut b, true).unwrap();
    assert_eq!(b[0], fake::pattern(1, 8 * 512));
    let ld = |a: &core::sync::atomic::AtomicU64| a.load(core::sync::atomic::Ordering::Relaxed);
    assert_eq!(ld(&c.stats.timeouts), 1);
    assert_eq!(ld(&c.stats.resets), 1);
    assert_eq!(hw.st.lock().enables, 2);
    // Two lost in a row: the retry also times out, the error reaches the caller, the controller still works.
    hw.st.lock().inject.drop_io = 2;
    assert_eq!(c.read(1, 8, &mut b, true), Err(CmdError::Timeout));
    c.read(1, 8, &mut b, true).unwrap();
    no_writes(&c, &hw);
}

#[test]
fn media_error_and_out_of_range() {
    let (c, hw) = up(vec![fake::ns(1, 9, 1024)]);
    hw.st.lock().inject.bad_lba = Some(100);
    let mut b = vec![0u8; 4096];
    let e = c.read(1, 96, &mut b, true).unwrap_err();
    assert_eq!(e, CmdError::Status { sct: 2, sc: 0x81, dnr: false });
    assert_eq!(e.to_dev(), DevError::Io);
    c.read(1, 200, &mut b, true).unwrap();
    // Past the end without the driver's own check: the device's LBA Out of Range.
    let e = c.read(1, 1020, &mut b, false).unwrap_err();
    assert_eq!(e, CmdError::Status { sct: 0, sc: 0x80, dnr: false });
    assert_eq!(e.to_dev(), DevError::InvalidArgument);
    let e = c.read(42, 0, &mut b[..512], false).unwrap_err();
    assert_eq!(e, CmdError::Status { sct: 0, sc: 0x0B, dnr: false });
    assert_eq!(c.state(), State::Ready);
}

#[test]
fn quiesce_shuts_down_then_restart_works() {
    let (c, hw) = up(vec![fake::ns(1, 9, 1024)]);
    c.quiesce();
    assert_eq!(c.state(), State::Quiesced);
    assert_eq!(c.quiesce_note() & 0x8000_0003, 0x8000_0003);
    {
        let st = hw.st.lock();
        assert_eq!(st.shutdowns, 1);
        assert_eq!(st.cc & regs::CC_EN, 0);
        assert_eq!(st.csts & regs::CSTS_RDY, 0);
        assert_eq!(st.pci_offs, 1);
    }
    let mut b = vec![0u8; 512];
    assert_eq!(c.read(1, 0, &mut b, true), Err(CmdError::Gone));
    for _ in 0..3 {
        c.restart().unwrap();
        c.read(1, 0, &mut b, true).unwrap();
        c.quiesce();
    }
    // A controller that never finishes its shutdown: Quiesce still ends (bounded) and disables it.
    c.restart().unwrap();
    hw.st.lock().inject.no_shutdown = true;
    c.quiesce();
    assert_eq!(c.quiesce_note() & 3, 2);
    no_writes(&c, &hw);
}

#[test]
fn not_ready_fails_bounded() {
    let (c, hw) = ctrl(vec![fake::ns(1, 9, 64)]);
    hw.st.lock().inject.never_ready = true;
    let t0 = hw.st.lock().now;
    let e = c.start().unwrap_err();
    assert!(e.contains("not ready"), "{e}");
    assert_eq!(c.state(), State::Failed);
    // CAP.TO = 20 -> 10 s, give or take the polling steps.
    let took = hw.st.lock().now - t0;
    assert!((10_000_000_000..12_000_000_000).contains(&took), "{took}");
    hw.st.lock().inject.never_ready = false;
    c.restart().unwrap();
}

#[test]
fn fatal_status_and_gone_device() {
    let (c, hw) = up(vec![fake::ns(1, 9, 64)]);
    hw.st.lock().inject.drop_io = 1;
    hw.st.lock().inject.fatal = true;
    let mut b = vec![0u8; 512];
    // Fatal -> reset -> the controller is still fatal: the bring-up fails, the error is the first one.
    assert_eq!(c.read(1, 0, &mut b, true), Err(CmdError::Fatal));
    assert_eq!(c.state(), State::Failed);
    hw.st.lock().inject.fatal = false;
    c.restart().unwrap();
    hw.st.lock().inject.gone = true;
    hw.st.lock().inject.drop_io = 1;
    assert_eq!(c.read(1, 0, &mut b, true), Err(CmdError::Gone));
    c.quiesce();
}

#[test]
fn hostile_namespace_is_unusable_not_a_panic() {
    let (c, hw) = ctrl(vec![fake::ns(1, 9, 64)]);
    hw.st.lock().inject.hostile_ns = true;
    c.start().unwrap();
    let i = c.info();
    assert_eq!(i.namespaces.len(), 1);
    assert!(i.namespaces[0].unusable.is_some());
    assert!(NsDevice::new(c.clone(), 1).is_none());
    let (c, _hw) = ctrl(vec![fake::FakeNs { ms: 8, ..fake::ns(1, 9, 64) }]);
    c.start().unwrap();
    assert!(c.info().namespaces[0].unusable.as_ref().unwrap().contains("metadata"));
}

#[test]
fn polled_mode_and_small_queues_wrap() {
    let (c, hw) = ctrl(vec![fake::ns(1, 12, 64)]);
    {
        let mut st = hw.st.lock();
        st.irq = IrqMode::Polled;
        st.cap = (st.cap & !0xFFFF) | 3;
        st.mdts = 0;
    }
    c.start().unwrap();
    let i = c.info();
    assert_eq!(i.io_depth, 4);
    assert_eq!(i.io_vector, None);
    // MDTS 0 = no limit: one PRP list page bounds it.
    assert_eq!(i.max_transfer, prp::max_bytes() - regs::PAGE);
    let mut b = vec![0u8; 4096];
    for k in 0..50u64 {
        c.read(1, k % 64, &mut b, true).unwrap();
        assert_eq!(b[17], fake::pattern(1, (k % 64) * 4096 + 17));
    }
    no_writes(&c, &hw);
}

#[test]
fn short_dma_memory_gives_smaller_transfers() {
    let (c, hw) = ctrl(vec![fake::ns(1, 9, 4096)]);
    hw.st.lock().fail_alloc_above = Some(64 * 1024);
    c.start().unwrap();
    assert_eq!(c.info().max_transfer, 64 * 1024);
    let mut b = vec![0u8; 200 * 1024];
    c.read(1, 1, &mut b, true).unwrap();
    assert_eq!(b[199 * 1024], fake::pattern(1, 512 + 199 * 1024));
}

#[test]
fn many_namespaces_need_no_fixed_limit() {
    let nss: Vec<_> = (1..=40u32).map(|n| fake::ns(n * 3, 9, 16)).collect();
    let (c, hw) = up(nss);
    assert_eq!(c.info().namespaces.len(), 40);
    let mut b = vec![0u8; 512];
    c.read(120, 15, &mut b, true).unwrap();
    assert_eq!(b[1], fake::pattern(120, 15 * 512 + 1));
    no_writes(&c, &hw);
}

#[test]
fn gpt_partitions_over_a_windows_namespace_refuse_writes() {
    use block::{Disk, DiskDevice, DiskKind, Dispatch};
    let bs = 512usize;
    let blocks = 8192u64;
    let img = block::testimg::gpt(
        bs,
        blocks,
        &[(block::table::GUID_EFI_SYSTEM, 2048, 4095), (block::table::GUID_MS_RESERVED, 4096, 4351), (block::table::GUID_MS_BASIC_DATA, 4352, 8000)],
    );
    let (c, hw) = up(vec![fake::FakeNs { nsid: 1, shift: 9, ms: 0, data: img.clone() }]);
    let dev: Arc<dyn BlockDevice> = Arc::new(NsDevice::new(c.clone(), 1).unwrap());
    let disk = Disk::open(dev, DiskKind::Nvme, Dispatch::Inline).unwrap();
    let scan = disk.scan();
    assert!(matches!(scan.verdict(), block::table::Verdict::Windows(_) | block::table::Verdict::Efi(_)), "{:?}", scan.verdict());
    assert_eq!(scan.parts.len(), 3);
    assert!(disk.write_policy().is_err());
    let lower: Arc<dyn BlockDevice> = Arc::new(DiskDevice(disk.clone()));
    let (parts, bad) = crate::gpt::partitions(&lower, "nvme9n1", &scan.parts);
    assert!(bad.is_empty());
    assert_eq!(parts.iter().map(|p| p.name()).collect::<Vec<_>>(), vec!["nvme9n1p1", "nvme9n1p2", "nvme9n1p3"]);
    let p3 = &parts[2];
    assert!(p3.info().read_only);
    let mut b = vec![0u8; bs];
    p3.read_blocks(0, &mut b, IoPriority::Normal).unwrap();
    assert_eq!(&b[..], &img[4352 * bs..4353 * bs]);
    assert!(p3.write_blocks(0, &b, true, IoPriority::Normal).is_err());
    let pd = Disk::open(Arc::new(crate::gpt::Partition::new(lower.clone(), "nvme9n1", 4352, 3649, 3, "data").unwrap()), DiskKind::Stacked, Dispatch::Inline).unwrap();
    assert!(pd.write(0, &b, true, IoPriority::Normal).is_err());
    assert!(disk.stats().refused >= 1);
    no_writes(&c, &hw);
    assert_eq!(hw.st.lock().ns[0].data, img, "the fake Windows namespace changed");
}

#[test]
fn the_only_opcodes_sent_are_the_builders() {
    let (c, hw) = up(vec![fake::ns(1, 9, 512), fake::ns(2, 12, 64)]);
    let mut b = vec![0u8; 8192];
    c.read(1, 0, &mut b, true).unwrap();
    c.flush(2).unwrap();
    let _ = c.get_feature(cmd::FEAT_VOLATILE_WC).unwrap();
    let _ = c.log_page(cmd::LOG_ERROR, 64).unwrap();
    let st = hw.st.lock();
    let allowed_admin: Vec<u8> = cmd::ADMIN_OPS.iter().map(|o| *o as u8).collect();
    let allowed_io: Vec<u8> = cmd::IO_OPS.iter().map(|o| *o as u8).collect();
    assert!(st.seen_admin.iter().all(|o| allowed_admin.contains(o)));
    assert!(st.seen_io.iter().all(|o| allowed_io.contains(o)));
    let (adm, io) = c.stats.used();
    assert_eq!(adm.iter().map(|x| x.1).sum::<u64>(), st.seen_admin.len() as u64);
    assert_eq!(io.iter().map(|x| x.1).sum::<u64>(), st.seen_io.len() as u64);
}

#[test]
fn a_controller_left_enabled_is_stopped_before_bus_master() {
    let (c, hw) = ctrl(vec![fake::ns(1, 9, 64)]);
    {
        let mut st = hw.st.lock();
        st.cc = regs::cc_config() | regs::CC_EN;
        st.csts = regs::CSTS_RDY;
    }
    c.start().unwrap();
    let calls = hw.st.lock().enable_calls.clone();
    assert_eq!(calls, vec![(false, true), (true, false)]);
    // Started again (nothing running): fine.
    assert!(c.start().is_ok());
}

fn ld(a: &core::sync::atomic::AtomicU64) -> u64 {
    a.load(core::sync::atomic::Ordering::Relaxed)
}

/// A Quiesce while the start is between its PCI steps wins: after `start` returns, CC.EN = 0 and bus master off.
#[test]
fn quiesce_during_start_wins() {
    for at in [fake::HookAt::Enable(false), fake::HookAt::Enable(true)] {
        let (c, hw) = ctrl(vec![fake::ns(1, 9, 1024)]);
        let q = c.clone();
        hw.hook(at, move || q.quiesce());
        assert!(c.start().is_err(), "{at:?}");
        assert_eq!(c.state(), State::Quiesced, "{at:?}");
        {
            let st = hw.st.lock();
            assert_eq!(st.cc & regs::CC_EN, 0, "{at:?}: controller left enabled");
            assert!(!st.bus_master, "{at:?}: bus master left on");
            assert_eq!(st.enables, 0, "{at:?}: the start enabled the controller after the Quiesce");
        }
        let mut b = vec![0u8; 512];
        assert!(c.read(1, 0, &mut b, true).is_err(), "never identified: no namespace yet");
        c.restart().unwrap();
        c.read(1, 0, &mut b, true).unwrap();
        assert!(hw.st.lock().bus_master);
        no_writes(&c, &hw);
    }
}

/// A Quiesce while a read is outstanding (its completion never comes): the read ends with Gone, no recovery
/// runs, the controller is off; a restart brings it back.
#[test]
fn quiesce_with_a_read_outstanding() {
    let (c, hw) = up(vec![fake::ns(1, 9, 1024)]);
    hw.st.lock().inject.drop_io = 1;
    let q = c.clone();
    hw.hook(fake::HookAt::IoDoorbell, move || q.quiesce());
    let mut b = vec![0u8; 4096];
    assert_eq!(c.read(1, 8, &mut b, true), Err(CmdError::Gone));
    assert_eq!(c.state(), State::Quiesced);
    assert_eq!(ld(&c.stats.resets), 0);
    {
        let st = hw.st.lock();
        assert_eq!(st.cc & regs::CC_EN, 0);
        assert!(!st.bus_master);
        assert_eq!(st.shutdowns, 1);
    }
    c.restart().unwrap();
    c.read(1, 8, &mut b, true).unwrap();
    assert_eq!(b[0], fake::pattern(1, 8 * 512));
    no_writes(&c, &hw);
}

/// Linux nvme_remap_bar: doorbells at the controller's stride must fit BAR0.
#[test]
fn doorbell_stride_beyond_the_bar_is_refused() {
    let (c, hw) = ctrl(vec![fake::ns(1, 9, 1024)]);
    {
        let mut st = hw.st.lock();
        st.cap |= 15u64 << 32;
        st.bar = 0x2000;
    }
    let e = c.start().unwrap_err();
    assert!(e.contains("doorbell stride"), "{e}");
    assert_eq!(c.state(), State::Failed);
    assert!(hw.st.lock().seen_admin.is_empty());
    // A wider stride that fits works (the fake decodes doorbells at the stride).
    let (c, hw) = ctrl(vec![fake::ns(1, 9, 1024)]);
    hw.st.lock().cap |= 2u64 << 32;
    c.start().unwrap();
    let mut b = vec![0u8; 4096];
    c.read(1, 3, &mut b, true).unwrap();
    assert_eq!(b[7], fake::pattern(1, 3 * 512 + 7));
}

/// Completions a broken controller might post: a CID no command has, an SQ head beyond the ring, the same
/// completion twice. No panic, no hang: strangers are counted and skipped, a lost completion recovers.
#[test]
fn hostile_completions_are_skipped_or_recovered() {
    let (c, hw) = up(vec![fake::ns(1, 9, 1024)]);
    let mut b = vec![0u8; 4096];
    hw.st.lock().inject.wrong_cid = 1;
    c.read(1, 8, &mut b, true).unwrap();
    assert_eq!(b[0], fake::pattern(1, 8 * 512));
    assert!(ld(&c.stats.stale) >= 1);
    assert_eq!(ld(&c.stats.resets), 1);
    hw.st.lock().inject.bad_sq_head = 5;
    for i in 0..5 {
        c.read(1, i * 8, &mut b, true).unwrap();
        assert_eq!(b[1], fake::pattern(1, i * 8 * 512 + 1));
    }
    let stale = ld(&c.stats.stale);
    hw.st.lock().inject.dup_io = 3;
    for i in 0..6 {
        c.read(1, i, &mut b[..512], true).unwrap();
        assert_eq!(b[0], fake::pattern(1, i * 512));
    }
    assert!(ld(&c.stats.stale) >= stale + 2);
    // Many small rings' worth of commands after all that: phase and head still in step.
    for i in 0..3000u64 {
        c.read(1, i % 1000, &mut b[..512], true).unwrap();
    }
    assert_eq!(c.state(), State::Ready);
    assert_eq!(ld(&c.stats.resets), 1);
    no_writes(&c, &hw);
}
