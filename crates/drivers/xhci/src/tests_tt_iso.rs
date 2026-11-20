//! GP-SOUND AU-2: a full-speed isochronous device behind a high-speed multi-TT hub, the JBL's place on the Gaming-PC
//! (root port 10 -> Genesys 05e3:0610 port 3, `plan/hw/gaming-pc-265k-2026-11-05/lsusb-hub-05e3-0610.txt`). Checked
//! against xHCI 1.2 6.2.2 (slot context: TT Hub Slot ID, TT Port Number, MTT; a hub's TT Think Time), 6.2.3.6
//! (full-speed isochronous Interval in frames + 3) and Linux 7.3.1 `drivers/usb/host/xhci-mem.c:1180-1185`,
//! `:1214-1236`, `:1410-1497` and `xhci-ring.c:2224-2226` (transaction and split errors do not halt isochronous
//! endpoints). What only the round shows stays unproven until a real round.

use crate::fake::Cfg;
use crate::fake_iso::IsoAct;
use crate::fake_usb::FakeDev;
use crate::hc::{Source, MS};
use crate::tests_usb::{devs, errors, settle, up};
use crate::trb::{cc, ty};
use alloc::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::vec::Vec;
use usb_core::{IsoOpts, IsoPacket, IsoRequest, IsoResult, Speed, UsbError, XferResult};

const OUT: u8 = 0x02;

#[test]
fn full_speed_isochronous_behind_a_multi_tt_hub() {
    let (c, env, mut hp) = up("t-tt-iso", Cfg::gaming_pc(), |m| {
        let mut h = FakeDev::hub(Speed::High, 4, true);
        h.plug(3, FakeDev::audio(Speed::Full));
        m.attach_root(10, h);
    });
    assert_eq!(errors(&env), Vec::<std::string::String>::new());
    let d = devs(&c);
    let hub = d.iter().find(|x| x.place.depth == 0 && x.speed() == Speed::High).expect("the hub");
    let jbl = d.iter().find(|x| x.vendor() == 0x0ECB).expect("the headset").clone();
    let hub_slot = (hub.slot.0 & 0xFF) as u8;
    let tt = |c: &crate::tests_usb::C| {
        let i = c.slot_info(jbl.slot).unwrap();
        (i.tt_slot, i.tt_port, i.mtt, i.root_port, i.speed)
    };
    let fs = Speed::Full.xhci_psiv();
    assert_eq!(tt(&c), (hub_slot, 3, true, 10, fs), "xHCI 6.2.2, xhci-mem.c:1180-1185");
    let hi = c.slot_info(hub.slot).unwrap();
    assert!(hi.hub && hi.mtt && hi.ports == 4);
    jbl.set_interface(1, 1).unwrap();
    assert_eq!(tt(&c), (hub_slot, 3, true, 10, fs), "Configure Endpoint keeps the TT fields of the slot");
    {
        let mut m = env.model();
        let out = m.usb.slots[&hub_slot].out;
        assert_eq!((m.out_get(out, 0, 2) >> 16) & 3, 2, "the hub's TT think time from wHubCharacteristics");
        let out = m.usb.slots[&((jbl.slot.0 & 0xFF) as u8)].out;
        let (e0, e1, e4) = (m.out_get(out, 4, 0), m.out_get(out, 4, 1), m.out_get(out, 4, 4));
        // Interval 3 (2^3 x 125 us = the 1 ms frame of bInterval 1), Mult 0, CErr 0, Isoch OUT, burst 0, 288 bytes.
        assert_eq!(((e0 >> 16) & 0xFF, (e0 >> 8) & 3, (e1 >> 1) & 3, (e1 >> 3) & 7, (e1 >> 8) & 0xFF, e1 >> 16), (3, 0, 0, 1, 0, 288));
        assert_eq!((e4 >> 16, e4 & 0xFFFF, e0 >> 24), (288, 288, 0), "Max ESIT Payload and average TRB length one packet");
    }
    let p = jbl.iso_pipe(OUT, IsoOpts::ms(8)).unwrap();
    assert_eq!((p.interval_ns(), p.max_packet()), (MS, 288));
    let log: Arc<StdMutex<Vec<IsoResult>>> = Default::default();
    let send = |n: usize| {
        let buf = jbl.alloc_buffer(192 * n).unwrap();
        let packets = (0..n).map(|i| IsoPacket { offset: 192 * i, len: 192 }).collect();
        let l = log.clone();
        p.queue(IsoRequest { buf, packets, frame: None }, alloc::boxed::Box::new(move |r| l.lock().unwrap().push(r))).unwrap();
    };
    let pump = || {
        env.model().pump();
        c.process_events(Source::Waiter);
    };
    send(4);
    pump();
    assert_eq!(log.lock().unwrap().last().map(|r| r.bytes()), Some(768));
    // A split transaction error: the packet ends with it, the endpoint runs on, and the worker that looks sends no
    // command (Linux 7.3.1 does not even look; BitsOS reads the context first, so the answer is the same).
    env.model().usb.iso.script.push_back(IsoAct::Error { code: cc::SPLIT_TRANSACTION, halt: false });
    send(3);
    pump();
    settle(&c, &env, &mut hp);
    pump();
    let last = log.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last.packets, [XferResult::failed(UsbError::Transfer(cc::SPLIT_TRANSACTION), 0), XferResult::ok(192), XferResult::ok(192)]);
    let m = env.model();
    assert!(!m.usb.commands.contains(&ty::RESET_ENDPOINT), "no Reset Endpoint for an endpoint that did not halt");
    assert!(m.usb.errors.is_empty(), "{:?}", m.usb.errors);
}
