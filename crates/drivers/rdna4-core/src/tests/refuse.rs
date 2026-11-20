use crate::ipfacts::{TABLE, hw};
use crate::refuse::*;
use crate::regmap::{self, PSP_C2PMSG_35, PSP_C2PMSG_64, Reg};

#[test]
fn the_flash_path_is_named_and_refused() {
    for command in [0x2, 0x3, 0x4] {
        assert!(is_spi_update(command));
    }
    assert!(!is_spi_update(0x11), "the read commands are not on this list");
    assert!(never_written(Reg { name: "c", hw_id: hw::MP0, base_index: 0, offset: 0xB3 }).is_some());
    assert!(never_written(Reg { name: "a", hw_id: hw::MP0, base_index: 0, offset: 0xB4 }).is_some());
    assert!(never_written(Reg { name: "d", hw_id: hw::MP0, base_index: 0, offset: 0x89 }).is_some());
}

#[test]
fn the_bootloader_mailbox_is_refused_the_ring_registers_are_not() {
    use crate::bootseq::Mailbox;
    assert_eq!(bootloader_mailbox(PSP_C2PMSG_35), Some(Mailbox::Command));
    assert_eq!(bootloader_mailbox(regmap::PSP_C2PMSG_36), Some(Mailbox::Address));
    assert_eq!(bootloader_mailbox(PSP_C2PMSG_64), None);
    // Without a gate nothing passes, whatever the value: one of Linux's nine commands, an address, anything.
    for (_, command) in crate::bootseq::SEQUENCE {
        assert!(write_refused(PSP_C2PMSG_35, command).is_some(), "{command:#x}");
        assert!(write_refused_at(PSP_C2PMSG_35, command, None).is_some(), "{command:#x}");
    }
    assert!(write_refused_at(regmap::PSP_C2PMSG_36, 0x8001, None).is_some());
    assert_eq!(never_written(PSP_C2PMSG_64), None);
    assert_eq!(never_written(regmap::PSP_C2PMSG_69), None);
    assert_eq!(never_written(regmap::PSP_C2PMSG_70), None);
    assert_eq!(never_written(regmap::PSP_C2PMSG_71), None);
    assert_eq!(never_written(regmap::SMU_MESSAGE), None);
}

/// A register is compared by where it is: an MP1 register at the same offset is not the MP0 one.
#[test]
fn the_same_offset_on_another_ip_is_another_register() {
    assert_eq!(never_written(Reg { name: "mp1", hw_id: hw::MP1, base_index: 1, offset: 0x63 }), None);
}

#[test]
fn every_never_written_register_has_a_place_in_the_table() {
    for (reg, why) in NEVER_WRITTEN {
        assert!(!why.is_empty());
        assert!(regmap::dword(&TABLE, reg).is_some(), "{}", reg.name);
    }
}

#[test]
fn the_bootloader_commands_by_name_and_the_nine_linux_sends() {
    assert!(is_bootloader_command(0x2_0000), "LOAD_SOSDRV");
    assert!(is_bootloader_command(0x1_0000), "LOAD_SYSDRV");
    assert!(is_bootloader_command(0x8_0000), "LOAD_KEY_DATABASE");
    assert_eq!(BOOTLOADER_COMMANDS.len(), 12);
    assert!(!is_bootloader_command(0));
    // The nine of the sequence are bootloader commands by Linux's names; the other three (two trainings, SPDM) are
    // in no sequence.
    for (part, command) in crate::bootseq::SEQUENCE {
        assert!(is_bootloader_command(command), "{part:?}");
    }
    let never: [u32; 3] = [0x0010_0000, 0x0020_0000, 0x2000_0000];
    for (c, name) in BOOTLOADER_COMMANDS {
        assert_eq!(never.contains(&c), !crate::bootseq::SEQUENCE.iter().any(|(_, s)| *s == c), "{name}");
    }
}

/// The gate decides only about the bootloader's two registers: everything else stays as refused as without it.
#[test]
fn a_gate_opens_nothing_but_the_bootloaders_mailbox() {
    let g = crate::bootseq::Gate::new(0x80_0000_0000..0x80_1000_0000);
    for reg in [regmap::PSP_C2PMSG_101, regmap::SMU_DEBUG_MESSAGE, NEVER_WRITTEN[0].0, NEVER_WRITTEN[1].0, NEVER_WRITTEN[2].0] {
        assert!(write_refused_at(reg, 0, Some(&g)).is_some(), "{}", reg.name);
    }
    assert!(write_refused_at(PSP_C2PMSG_64, GFX_CTRL_MODE1_RESET, Some(&g)).is_some());
    assert_eq!(write_refused_at(PSP_C2PMSG_64, GFX_CTRL_INIT_GPCOM_RING, Some(&g)), None);
    assert!(write_refused_at(regmap::SMU_MESSAGE, 0x32, Some(&g)).is_some());
    // A fresh gate has seen nothing: the first command is refused too.
    assert!(write_refused_at(PSP_C2PMSG_35, 0x8_0000, Some(&g)).is_some());
}

#[test]
fn mode1_reset_is_not_one_of_the_ring_commands_a_driver_needs() {
    // INIT_GPCOM_RING and DESTROY_RINGS are what a driver uses of this register (A:psp_gfx_if.h:43-44).
    assert_ne!(GFX_CTRL_MODE1_RESET, 0x2_0000);
    assert_ne!(GFX_CTRL_MODE1_RESET, 0x3_0000);
    assert!(!is_bootloader_command(GFX_CTRL_MODE1_RESET));
}

#[test]
fn power_limit_voltage_and_reset_messages_are_never_sent() {
    for (message, name) in [
        (0x32, "SetPptLimit"),
        (0x1C, "SetHardMaxByFreq"),
        (0x1A, "SetSoftMaxByFreq"),
        (0x3A, "SetThrottlerMask"),
        (0x4F, "Mode3Reset"),
        (0x15, "EnterBaco"),
        (0x13, "TransferTableDram2Smu"),
    ] {
        assert_eq!(smu_verdict(message), Verdict::Never(name), "{message:#x}");
    }
}

#[test]
fn the_questions_are_allowed() {
    assert_eq!(smu_verdict(0x2), Verdict::Allowed("GetSmuVersion"));
    assert_eq!(smu_verdict(0x3), Verdict::Allowed("GetDriverIfVersion"));
    assert_eq!(smu_verdict(0x33), Verdict::Allowed("GetPptLimit"));
    assert_eq!(smu_verdict(0x54), Verdict::Allowed("GetAllRunningSmuFeatures"));
    // Every question is a question by name: nothing named Set, Enable, Run or Transfer slipped in.
    for (_, name) in SMU_QUESTIONS {
        assert!(name.starts_with("Get") || name == "TestMessage", "{name}");
    }
}

#[test]
fn what_nobody_reviewed_is_not_allowed() {
    assert_eq!(smu_verdict(0x28), Verdict::NotReviewed, "AllowGfxOff");
    assert_eq!(smu_verdict(0x04), Verdict::NotReviewed, "SetAllowedFeaturesMaskLow: Linux sends it only without SCPM");
    assert_eq!(smu_verdict(0x16), Verdict::NotReviewed, "ExitBaco");
    assert_eq!(smu_verdict(0x00), Verdict::NotReviewed);
    assert_eq!(smu_verdict(SMU_MESSAGE_COUNT), Verdict::Unknown);
    assert_eq!(smu_verdict(u16::MAX), Verdict::Unknown);
}

#[test]
fn no_message_is_on_both_lists_and_all_are_real() {
    for (q, name) in SMU_QUESTIONS {
        assert!(q < SMU_MESSAGE_COUNT, "{name}");
        assert!(SMU_NEVER.iter().all(|(n, _)| *n != q), "{name}");
    }
    for (n, name) in SMU_NEVER {
        assert!(n < SMU_MESSAGE_COUNT, "{name}");
    }
}

#[test]
fn only_ring_create_and_destroy_reach_the_ring_control() {
    assert_eq!(write_refused(PSP_C2PMSG_64, GFX_CTRL_INIT_GPCOM_RING), None);
    assert_eq!(write_refused(PSP_C2PMSG_64, GFX_CTRL_DESTROY_RINGS), None);
    assert!(write_refused(PSP_C2PMSG_64, GFX_CTRL_MODE1_RESET).is_some());
    for v in [0, 0x1_0000, 0x4_0000, 0x8_0000, 0x2_0001] {
        assert!(write_refused(PSP_C2PMSG_64, v).is_some(), "{v:#x}");
    }
}

#[test]
fn the_flash_bootloader_sriov_and_debug_registers_refuse_every_value() {
    for reg in [PSP_C2PMSG_35, regmap::PSP_C2PMSG_36, regmap::PSP_C2PMSG_101, regmap::PSP_C2PMSG_102, regmap::PSP_C2PMSG_103, regmap::SMU_DEBUG_MESSAGE, regmap::SMU_DEBUG_ARGUMENT, regmap::SMU_DEBUG_ANSWER] {
        for v in [0, 1, 0x2_0000, u32::MAX] {
            assert!(write_refused(reg, v).is_some(), "{} {v:#x}", reg.name);
        }
    }
    for (reg, _) in NEVER_WRITTEN {
        assert!(write_refused(reg, 0).is_some(), "{}", reg.name);
    }
}

#[test]
fn only_reviewed_questions_reach_the_smu() {
    for (q, name) in SMU_QUESTIONS {
        assert_eq!(write_refused(regmap::SMU_MESSAGE, u32::from(q)), None, "{name}");
    }
    for (n, name) in SMU_NEVER {
        assert!(write_refused(regmap::SMU_MESSAGE, u32::from(n)).is_some(), "{name}");
    }
    assert!(write_refused(regmap::SMU_MESSAGE, 0x28).is_some(), "AllowGfxOff is not reviewed");
    assert!(write_refused(regmap::SMU_MESSAGE, 0x1_0002).is_some(), "upper bits name no message");
    // The answer and argument registers are the protocol's: a stage's list decides about them.
    assert_eq!(write_refused(regmap::SMU_ANSWER, 0), None);
    assert_eq!(write_refused(regmap::SMU_ARGUMENT, 0x1234), None);
}

#[test]
fn only_linuxs_ring_commands_and_image_types_go_on_the_ring() {
    for (id, _) in RING_COMMANDS {
        let words = if id == 0x06 { [0, 0, 4096, 8] } else { [0; 4] };
        assert_eq!(ring_command_refused(id, &words), None, "{id:#x}");
    }
    assert_eq!(RING_COMMANDS.map(|(id, _)| id), [0x01, 0x04, 0x06, 0x20, 0x21, 0x50, 0x51]);
    // BOOT_CFG, SRIOV_SPATIAL_PART, FB_NPS_MODE, UNLOAD_TA, INVOKE_CMD, SETUP_TMR, DESTROY_TMR, and an id nobody
    // named.
    for id in [0x22, 0x27, 0x48, 0x02, 0x03, 0x05, 0x07, 0x99] {
        assert!(ring_command_refused(id, &[0; 4]).is_some(), "{id:#x}");
    }
    for (t, _) in IP_FW_TYPES {
        assert_eq!(ring_command_refused(0x06, &[0, 0, 4096, t]), None, "type {t}");
    }
    // What Linux loads on this card besides GC, SDMA, MES and the SMU: VCN, DMUB, the register list.
    for t in [13, 51, 67] {
        assert!(IP_FW_TYPES.iter().any(|(x, _)| *x == t), "type {t}");
    }
    // DMCU, the SRM control list (0 bytes in this card's RLC file is still a known type), a type past the list, and
    // a LOAD_IP_FW without its type word.
    for t in [11, 12, 96] {
        assert!(ring_command_refused(0x06, &[0, 0, 4096, t]).is_some(), "type {t}");
    }
    assert!(ring_command_refused(0x06, &[0, 0, 4096]).is_some());
}
