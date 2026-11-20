use crate::bootseq::*;

const WINDOW: core::ops::Range<u64> = 0x80_0120_0000..0x80_0520_0000;
const BUF: u64 = 0x80_0130_0000;

fn word() -> u32 {
    address_word(BUF, &WINDOW).unwrap()
}

/// Linux's reads before a part: the sign of life (0), the bootloader's ready bit.
fn ready(g: &mut Gate) {
    g.saw_sign_of_life(0);
    g.saw_bootloader(BOOTLOADER_READY);
}

/// One part as Linux loads it; panics where the gate refuses.
fn load(g: &mut Gate, command: u32) {
    ready(g);
    assert_eq!(g.refused(Mailbox::Address, word()), None);
    g.wrote(Mailbox::Address);
    assert_eq!(g.refused(Mailbox::Command, command), None, "{command:#x}");
    g.wrote(Mailbox::Command);
}

#[test]
fn the_sequence_is_linuxs_nine_in_linuxs_order() {
    // psp_hw_start, amdgpu_psp.c:2961-3048, and the trace's nine values (init.txt 676, 685, 694, 703, 712, 721, 730,
    // 739, 748).
    let commands: [u32; 9] = SEQUENCE.map(|(_, c)| c);
    assert_eq!(commands, [0x0008_0000, 0x1000_0000, 0x0001_0000, 0x000B_0000, 0x000D_0000, 0x000C_0000, 0x000E_0000, 0x000F_0000, 0x0002_0000]);
    let parts: [Part; 9] = SEQUENCE.map(|(p, _)| p);
    assert_eq!(parts, [Part::Kdb, Part::Spl, Part::SysDrv, Part::SocDrv, Part::IntfDrv, Part::DbgDrv, Part::RasDrv, Part::IpKeyMgr, Part::Sos]);
    // The descriptor types of amdgpu_ucode.h:118-133.
    assert_eq!(parts.map(|p| p as u32), [3, 5, 2, 7, 8, 9, 10, 11, 1]);
    for (i, (p, _)) in SEQUENCE.into_iter().enumerate() {
        assert!(!p.name().is_empty() && SEQUENCE[..i].iter().all(|(q, _)| q.name() != p.name()), "{p:?}");
    }
}

#[test]
fn the_nine_pass_in_order_and_then_the_gate_is_shut() {
    let mut g = Gate::new(WINDOW);
    for (i, (part, command)) in SEQUENCE.into_iter().enumerate() {
        assert_eq!((g.sent(), g.next()), (i, Some((part, command))));
        load(&mut g, command);
    }
    assert_eq!((g.sent(), g.next()), (9, None));
    ready(&mut g);
    for (_, command) in SEQUENCE {
        assert_eq!(g.refused(Mailbox::Command, command), Some("all nine bootloader commands went out in this boot"));
    }
    assert!(g.refused(Mailbox::Address, word()).is_some());
}

#[test]
fn a_command_out_of_order_is_refused_at_every_position() {
    for at in 0..SEQUENCE.len() {
        let mut g = Gate::new(WINDOW);
        for (_, c) in &SEQUENCE[..at] {
            load(&mut g, *c);
        }
        ready(&mut g);
        assert_eq!(g.refused(Mailbox::Address, word()), None);
        g.wrote(Mailbox::Address);
        for (i, (part, c)) in SEQUENCE.into_iter().enumerate() {
            let r = g.refused(Mailbox::Command, c);
            match i.cmp(&at) {
                core::cmp::Ordering::Less => assert_eq!(r, Some("this bootloader command went out already"), "{part:?} at {at}"),
                core::cmp::Ordering::Equal => assert_eq!(r, None, "{part:?} at {at}"),
                core::cmp::Ordering::Greater => assert_eq!(r, Some("a bootloader command out of Linux's order"), "{part:?} at {at}"),
            }
        }
    }
}

#[test]
fn a_command_twice_is_refused() {
    let mut g = Gate::new(WINDOW);
    load(&mut g, 0x8_0000);
    // Right away, and after Linux's reads for the next part.
    assert!(g.refused(Mailbox::Command, 0x8_0000).is_some());
    ready(&mut g);
    assert!(g.refused(Mailbox::Address, word()).is_none());
    g.wrote(Mailbox::Address);
    assert_eq!(g.refused(Mailbox::Command, 0x8_0000), Some("this bootloader command went out already"));
    assert_eq!(g.sent(), 1);
}

#[test]
fn values_outside_the_nine_are_refused_by_what_they_are() {
    let mut g = Gate::new(WINDOW);
    ready(&mut g);
    g.wrote(Mailbox::Address);
    let named = [
        (0x0010_0000, "memory training is never sent (Linux did not train this card)"),
        (0x0020_0000, "memory training is never sent (Linux did not train this card)"),
        (0x2000_0000, "the SPDM driver is not loaded by PSP 14.0"),
        (0x0480_0000, "the USB-PD firmware load"),
        (0x0200_0000, "the USB-PD firmware version command"),
        (0, "not one of Linux's nine bootloader commands"),
        (0x0008_0001, "not one of Linux's nine bootloader commands"),
        (0x0003_0000, "not one of Linux's nine bootloader commands"),
        (0x0007_0000, "not one of Linux's nine bootloader commands"),
        (u32::MAX, "not one of Linux's nine bootloader commands"),
    ];
    for (v, why) in named {
        assert_eq!(g.refused(Mailbox::Command, v), Some(why), "{v:#x}");
    }
    // The flash mailbox's command numbers (psp_v14_0.c:54-56) name nothing here either.
    for (v, _) in crate::refuse::SPI_UPDATE_COMMANDS {
        assert!(g.refused(Mailbox::Command, v).is_some());
    }
    // Every single-bit value and every 16-bit command field but the nine.
    for bit in 0..32 {
        let v = 1u32 << bit;
        assert_eq!(g.refused(Mailbox::Command, v).is_none(), v == 0x8_0000, "{v:#x}");
    }
    for field in 0..=0xFFFFu32 {
        let v = field << 16;
        assert_eq!(g.refused(Mailbox::Command, v).is_none(), v == 0x8_0000, "{v:#x}");
    }
}

#[test]
fn each_step_needs_the_one_before() {
    let mut g = Gate::new(WINDOW);
    // Nothing read yet.
    assert_eq!(g.refused(Mailbox::Address, word()), Some("the bootloader's address before C2PMSG_81 was read as 0"));
    assert_eq!(g.refused(Mailbox::Command, 0x8_0000), Some("a bootloader command before C2PMSG_81 was read as 0"));
    // The bootloader's ready bit before the sign of life counts for nothing.
    g.saw_bootloader(BOOTLOADER_READY);
    assert!(g.refused(Mailbox::Address, word()).is_some());
    g.saw_sign_of_life(0);
    assert_eq!(g.refused(Mailbox::Address, word()), Some("the bootloader's address before its ready bit was read"));
    assert_eq!(g.refused(Mailbox::Command, 0x8_0000), Some("a bootloader command before the bootloader's ready bit was read"));
    // Not ready: the echo of a command, 0, and a card that reads all ones.
    for v in [0x0008_0000, 0, 0x7FFF_FFFF, u32::MAX] {
        g.saw_bootloader(v);
        assert!(g.refused(Mailbox::Address, word()).is_some(), "{v:#x}");
    }
    g.saw_bootloader(BOOTLOADER_READY);
    assert_eq!(g.refused(Mailbox::Command, 0x8_0000), Some("a bootloader command before its buffer's address"));
    assert_eq!(g.refused(Mailbox::Address, word()), None);
    g.wrote(Mailbox::Address);
    assert_eq!(g.refused(Mailbox::Address, word()), Some("the bootloader's address a second time"));
    assert_eq!(g.refused(Mailbox::Command, 0x8_0000), None);
    g.wrote(Mailbox::Command);
    // After a command the next part starts from the sign of life again: the answer wait's ready bit alone is not
    // enough.
    g.saw_bootloader(BOOTLOADER_READY);
    assert_eq!(g.refused(Mailbox::Address, word()), Some("the bootloader's address before C2PMSG_81 was read as 0"));
}

#[test]
fn once_the_secure_os_was_seen_alive_nothing_passes_again() {
    for sol in [0x0E5B_5A34, 1, u32::MAX] {
        let mut g = Gate::new(WINDOW);
        load(&mut g, 0x8_0000);
        ready(&mut g);
        g.saw_sign_of_life(sol);
        assert!(g.alive());
        // Even when it reads 0 again later.
        ready(&mut g);
        for to in [Mailbox::Address, Mailbox::Command] {
            for v in [word(), 0x1000_0000, 0x2_0000, 0] {
                assert_eq!(g.refused(to, v), Some("the secure OS runs: nothing goes to the bootloader any more"), "{sol:#x} {to:?} {v:#x}");
            }
        }
    }
    // A warm card: alive at the first read, before anything.
    let mut g = Gate::new(WINDOW);
    g.saw_sign_of_life(0x0E5B_5A34);
    g.saw_bootloader(BOOTLOADER_READY);
    assert!(g.refused(Mailbox::Address, word()).is_some() && g.refused(Mailbox::Command, 0x8_0000).is_some());
}

#[test]
fn the_address_must_name_an_aligned_megabyte_inside_the_window() {
    assert_eq!(address_word(BUF, &WINDOW), Ok(0x8_0013));
    assert_eq!(address_word(WINDOW.start + (3 << 20), &WINDOW), Ok(0x8_0015));
    assert_eq!(address_word(WINDOW.end - (1 << 20), &WINDOW), Ok(0x8_0051));
    for off in [1, 0x1000, 0x8_0000, 0xF_FFFF] {
        assert_eq!(address_word(BUF + off, &WINDOW), Err("the firmware buffer is not 1 MiB aligned"), "{off:#x}");
    }
    for mc in [0, 0x30_0000, WINDOW.start - (1 << 20), WINDOW.end, 0x80_0000_0000] {
        assert_eq!(address_word(mc, &WINDOW), Err("the firmware buffer lies outside the window"), "{mc:#x}");
    }
    // A megabyte that only begins inside.
    let odd = WINDOW.start..WINDOW.end - 0x1000;
    assert!(address_word(odd.end - 0xF_F000 - 0x1000, &odd).is_err());
    assert!(address_word(u64::MAX & !0xF_FFFF, &(0..u64::MAX)).is_err(), "overflow");
    assert!(address_word(1 << 52, &(0..u64::MAX)).is_err(), "past the register");
    // The gate holds the written word to the same rule: Linux's own 0x3 (its GART address) is outside the window.
    let mut g = Gate::new(WINDOW);
    ready(&mut g);
    for w in [0x3, 0, 0x8_0011, 0x8_0052, u32::MAX] {
        assert_eq!(g.refused(Mailbox::Address, w), Some("the firmware buffer lies outside the window"), "{w:#x}");
    }
    for w in [0x8_0012, 0x8_0013, 0x8_0051] {
        assert_eq!(g.refused(Mailbox::Address, w), None, "{w:#x}");
    }
}
