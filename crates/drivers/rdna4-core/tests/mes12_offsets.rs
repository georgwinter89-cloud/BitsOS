//! Wave 44, item ME: `rdna4_core::mes12api`'s and `rdna4_core::mqd12`'s dword offsets against Linux 7.3.1's own
//! headers (`include/mes_v12_api_def.h`, pack 8; `include/v12_structs.h`): a C file that includes them and asserts
//! every offset, size and flag value at compile time (`_Static_assert`), checked by the host's C compiler with
//! `-fsyntax-only` (nothing is built or run). MES reads a frame and the CP an MQD by exactly these compiled offsets,
//! so a number here that is not the compiler's would put a field where the firmware reads another one.
//!
//! The tree is the desk's read-only copy, as `tests/pptable_offsets.rs` finds it. Without a tree or a C compiler
//! the test fails and says so: it never passes unchecked.

use rdna4_core::mes12api::{self as api, at};
use rdna4_core::mqd12::{c, g};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;

const AMD: &str = "drivers/gpu/drm/amd";

fn linux() -> PathBuf {
    let mut tried = Vec::new();
    let home = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("BitsOS/V2/ref/linux-7.3.1"));
    for dir in [std::env::var_os("LINUX_REF").map(PathBuf::from), Some(PathBuf::from("/var/tmp/claude-v2/ref/linux-7.3.1")), home].into_iter().flatten() {
        if dir.join(AMD).join("include/mes_v12_api_def.h").exists() {
            return dir;
        }
        tried.push(dir.display().to_string());
    }
    panic!("no Linux 7.3.1 tree with {AMD}/include/mes_v12_api_def.h (tried {tried:?}; set LINUX_REF to one)");
}

fn source() -> String {
    let mut s = String::from("#include <stdint.h>\n#include <stddef.h>\n#include <stdbool.h>\ntypedef uint64_t uint64;\n#include \"mes_v12_api_def.h\"\n#include \"v12_structs.h\"\n");
    let mut dw = |ty: &str, field: &str, want: usize| writeln!(s, "_Static_assert(offsetof({ty}, {field}) == {} , \"{ty}.{field} is not at dword {want}\");", want * 4).unwrap();
    let hw = "union MESAPI_SET_HW_RESOURCES";
    for (f, w) in [
        ("vmid_mask_mmhub", at::HW_VMID_MMHUB),
        ("vmid_mask_gfxhub", at::HW_VMID_GFXHUB),
        ("gds_size", at::HW_GDS_SIZE),
        ("paging_vmid", at::HW_PAGING_VMID),
        ("compute_hqd_mask", at::HW_COMPUTE_HQD),
        ("gfx_hqd_mask", at::HW_GFX_HQD),
        ("sdma_hqd_mask", at::HW_SDMA_HQD),
        ("aggregated_doorbells", at::HW_AGG_DOORBELLS),
        ("g_sch_ctx_gpu_mc_ptr", at::HW_SCH_CTX),
        ("query_status_fence_gpu_mc_ptr", at::HW_QUERY_FENCE),
        ("gc_base", at::HW_GC_BASE),
        ("mmhub_base", at::HW_MMHUB_BASE),
        ("osssys_base", at::HW_OSSSYS_BASE),
        ("api_status", at::HW_STATUS),
        ("uint32_all", at::HW_FLAGS),
        ("oversubscription_timer", at::HW_OVERSUB_TIMER),
    ] {
        dw(hw, f, w);
    }
    let hw1 = "union MESAPI_SET_HW_RESOURCES_1";
    for (f, w) in [("api_status", at::HW1_STATUS), ("uint32_all", at::HW1_FLAGS), ("mes_kiq_unmap_timeout", at::HW1_KIQ_UNMAP_TIMEOUT), ("cleaner_shader_fence_mc_addr", at::HW1_CLEANER_FENCE)] {
        dw(hw1, f, w);
    }
    let add = "union MESAPI__ADD_QUEUE";
    for (f, w) in [
        ("process_id", at::ADD_PROCESS_ID),
        ("gang_context_addr", at::ADD_GANG_CTX),
        ("doorbell_offset", at::ADD_DOORBELL),
        ("mqd_addr", at::ADD_MQD),
        ("wptr_addr", at::ADD_WPTR),
        ("queue_type", at::ADD_QUEUE_TYPE),
        ("vm_context_cntl", at::ADD_VM_CONTEXT_CNTL),
        ("api_status", at::ADD_STATUS),
        ("pipe_id", at::ADD_PIPE),
        ("queue_id", at::ADD_QUEUE),
    ] {
        dw(add, f, w);
    }
    let rm = "union MESAPI__REMOVE_QUEUE";
    for (f, w) in [
        ("doorbell_offset", at::REMOVE_DOORBELL),
        ("gang_context_addr", at::REMOVE_GANG_CTX),
        ("api_status", at::REMOVE_STATUS),
        ("pipe_id", at::REMOVE_PIPE),
        ("queue_id", at::REMOVE_QUEUE),
        ("tf_addr", at::REMOVE_TF_ADDR),
        ("tf_data", at::REMOVE_TF_DATA),
        ("queue_type", at::REMOVE_QUEUE_TYPE),
    ] {
        dw(rm, f, w);
    }
    dw("union MESAPI__QUERY_MES_STATUS", "subopcode", at::QUERY_SUBOPCODE);
    dw("union MESAPI__QUERY_MES_STATUS", "api_status", at::QUERY_STATUS);
    let misc = "union MESAPI__MISC";
    for (f, w) in [
        ("opcode", at::MISC_OPCODE),
        ("api_status", at::MISC_STATUS),
        ("write_reg.reg_offset", at::MISC_DATA),
        ("write_reg.reg_value", at::MISC_WRITE_VALUE),
        ("read_reg.reg_offset", at::MISC_DATA),
        ("read_reg.buffer_addr", at::MISC_READ_BUFFER),
        ("wait_reg_mem.op", at::MISC_DATA),
        ("wait_reg_mem.reference", at::MISC_WRM_REFERENCE),
        ("wait_reg_mem.mask", at::MISC_WRM_MASK),
        ("wait_reg_mem.reg_offset1", at::MISC_WRM_REG1),
        ("wait_reg_mem.reg_offset2", at::MISC_WRM_REG2),
        ("change_config.opcode", at::MISC_DATA),
        ("change_config.option", at::MISC_CHANGE_CONFIG_OPTION),
    ] {
        dw(misc, f, w);
    }
    for (f, w) in [
        ("header", c::HEADER),
        ("compute_pipelinestat_enable", c::COMPUTE_PIPELINESTAT_ENABLE),
        ("compute_static_thread_mgmt_se0", c::STATIC_THREAD_MGMT_SE0),
        ("compute_static_thread_mgmt_se1", c::STATIC_THREAD_MGMT_SE1),
        ("compute_static_thread_mgmt_se2", c::STATIC_THREAD_MGMT_SE2),
        ("compute_static_thread_mgmt_se3", c::STATIC_THREAD_MGMT_SE3),
        ("compute_misc_reserved", c::COMPUTE_MISC_RESERVED),
        ("cp_mqd_base_addr_lo", c::MQD_BASE_ADDR_LO),
        ("cp_mqd_base_addr_hi", c::MQD_BASE_ADDR_HI),
        ("cp_hqd_active", c::HQD_ACTIVE),
        ("cp_hqd_vmid", c::HQD_VMID),
        ("cp_hqd_persistent_state", c::HQD_PERSISTENT_STATE),
        ("cp_hqd_pipe_priority", c::HQD_PIPE_PRIORITY),
        ("cp_hqd_queue_priority", c::HQD_QUEUE_PRIORITY),
        ("cp_hqd_quantum", c::HQD_QUANTUM),
        ("cp_hqd_pq_base_lo", c::HQD_PQ_BASE_LO),
        ("cp_hqd_pq_base_hi", c::HQD_PQ_BASE_HI),
        ("cp_hqd_pq_rptr", c::HQD_PQ_RPTR),
        ("cp_hqd_pq_rptr_report_addr_lo", c::HQD_PQ_RPTR_REPORT_ADDR_LO),
        ("cp_hqd_pq_rptr_report_addr_hi", c::HQD_PQ_RPTR_REPORT_ADDR_HI),
        ("cp_hqd_pq_wptr_poll_addr_lo", c::HQD_PQ_WPTR_POLL_ADDR_LO),
        ("cp_hqd_pq_wptr_poll_addr_hi", c::HQD_PQ_WPTR_POLL_ADDR_HI),
        ("cp_hqd_pq_doorbell_control", c::HQD_PQ_DOORBELL_CONTROL),
        ("cp_hqd_pq_control", c::HQD_PQ_CONTROL),
        ("cp_hqd_ib_control", c::HQD_IB_CONTROL),
        ("cp_hqd_iq_timer", c::HQD_IQ_TIMER),
        ("cp_hqd_dequeue_request", c::HQD_DEQUEUE_REQUEST),
        ("cp_mqd_control", c::MQD_CONTROL),
        ("cp_hqd_eop_base_addr_lo", c::HQD_EOP_BASE_ADDR_LO),
        ("cp_hqd_eop_base_addr_hi", c::HQD_EOP_BASE_ADDR_HI),
        ("cp_hqd_eop_control", c::HQD_EOP_CONTROL),
        ("cp_hqd_pq_wptr_lo", c::HQD_PQ_WPTR_LO),
        ("cp_hqd_pq_wptr_hi", c::HQD_PQ_WPTR_HI),
        ("reserved_184", c::RESERVED_184),
        ("fence_address_lo", c::FENCE_ADDRESS_LO),
        ("fence_address_hi", c::FENCE_ADDRESS_HI),
    ] {
        dw("struct v12_compute_mqd", f, w);
    }
    for (f, w) in [
        ("shadow_base_lo", g::SHADOW_BASE_LO),
        ("shadow_base_hi", g::SHADOW_BASE_HI),
        ("fw_work_area_base_lo", g::FW_WORK_AREA_BASE_LO),
        ("fw_work_area_base_hi", g::FW_WORK_AREA_BASE_HI),
        ("cp_mqd_base_addr", g::MQD_BASE_ADDR),
        ("cp_mqd_base_addr_hi", g::MQD_BASE_ADDR_HI),
        ("cp_gfx_hqd_active", g::HQD_ACTIVE),
        ("cp_gfx_hqd_vmid", g::HQD_VMID),
        ("cp_gfx_hqd_queue_priority", g::HQD_QUEUE_PRIORITY),
        ("cp_gfx_hqd_quantum", g::HQD_QUANTUM),
        ("cp_gfx_hqd_base", g::HQD_BASE),
        ("cp_gfx_hqd_base_hi", g::HQD_BASE_HI),
        ("cp_gfx_hqd_rptr", g::HQD_RPTR),
        ("cp_gfx_hqd_rptr_addr", g::HQD_RPTR_ADDR),
        ("cp_gfx_hqd_rptr_addr_hi", g::HQD_RPTR_ADDR_HI),
        ("cp_rb_wptr_poll_addr_lo", g::RB_WPTR_POLL_ADDR_LO),
        ("cp_rb_wptr_poll_addr_hi", g::RB_WPTR_POLL_ADDR_HI),
        ("cp_rb_doorbell_control", g::RB_DOORBELL_CONTROL),
        ("cp_gfx_hqd_cntl", g::HQD_CNTL),
        ("cp_gfx_hqd_wptr", g::HQD_WPTR),
        ("cp_gfx_hqd_wptr_hi", g::HQD_WPTR_HI),
        ("cp_gfx_mqd_control", g::MQD_CONTROL),
        ("fence_address_lo", g::FENCE_ADDRESS_LO),
        ("fence_address_hi", g::FENCE_ADDRESS_HI),
    ] {
        dw("struct v12_gfx_mqd", f, w);
    }
    let mut is = |expr: &str, want: u64| writeln!(s, "_Static_assert(({expr}) == {want}u, \"{expr} is not {want}\");").unwrap();
    for u in ["MESAPI_SET_HW_RESOURCES", "MESAPI_SET_HW_RESOURCES_1", "MESAPI__ADD_QUEUE", "MESAPI__REMOVE_QUEUE", "MESAPI__QUERY_MES_STATUS", "MESAPI__MISC"] {
        is(&format!("sizeof(union {u})"), u64::from(api::FRAME_BYTES));
    }
    is("sizeof(struct v12_compute_mqd)", u64::from(rdna4_core::mqd12::MQD_BYTES));
    is("sizeof(struct v12_gfx_mqd)", u64::from(rdna4_core::mqd12::MQD_BYTES));
    is("API_FRAME_SIZE_IN_DWORDS", api::FRAME_DW as u64);
    is("MES_API_VERSION", u64::from(api::HEADER_API_VERSION));
    is("MES_SCH_API_MAX", u64::from(api::OPCODE_MAX));
    for (name, v) in [
        ("MES_SCH_API_SET_HW_RSRC", api::op::SET_HW_RSRC),
        ("MES_SCH_API_ADD_QUEUE", api::op::ADD_QUEUE),
        ("MES_SCH_API_REMOVE_QUEUE", api::op::REMOVE_QUEUE),
        ("MES_SCH_API_QUERY_SCHEDULER_STATUS", api::op::QUERY_SCHEDULER_STATUS),
        ("MES_SCH_API_MISC", api::op::MISC),
        ("MES_SCH_API_SET_HW_RSRC_1", api::op::SET_HW_RSRC_1),
        ("MES_SCH_API_INV_TLBS", api::op::INV_TLBS),
        ("MESAPI_MISC__WRITE_REG", api::misc::WRITE_REG),
        ("MESAPI_MISC__READ_REG", api::misc::READ_REG),
        ("MESAPI_MISC__WAIT_REG_MEM", api::misc::WAIT_REG_MEM),
        ("MESAPI_MISC__CHANGE_CONFIG", api::misc::CHANGE_CONFIG),
        ("WRM_OPERATION__WR_WAIT_WR_REG", api::misc::WRM_WR_WAIT_WR_REG),
        ("MESAPI_MISC__CHANGE_CONFIG_OPTION_LIMIT_SINGLE_PROCESS", api::misc::CHANGE_CONFIG_LIMIT_SINGLE_PROCESS),
        ("MES_QUEUE_TYPE_GFX", api::qtype::GFX),
        ("MES_QUEUE_TYPE_COMPUTE", api::qtype::COMPUTE),
        ("MES_QUEUE_TYPE_SCHQ", api::qtype::SCHQ),
    ] {
        is(name, u64::from(v));
    }
    // The flag words as the compiler lays out the bitfields Linux sets.
    s.push_str(
        "static const union MESAPI_SET_HW_RESOURCES hw = { .disable_reset = 1, .disable_mes_log = 1, .use_different_vmid_compute = 1, .enable_reg_active_poll = 1, .enable_level_process_quantum_check = 1, .unmapped_doorbell_handling = 2 };\n\
         static const union MESAPI__ADD_QUEUE add = { .map_legacy_kq = 1 };\n\
         static const union MESAPI__REMOVE_QUEUE unmap = { .unmap_legacy_queue = 1, .remove_queue_after_reset = 1 };\n\
         static const union MESAPI__REMOVE_QUEUE preempt = { .preempt_legacy_gfx_queue = 1 };\n",
    );
    s
}

fn compile(name: &str, c: &str) -> std::process::Output {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let file = dir.join(name);
    std::fs::write(&file, c).unwrap();
    Command::new("cc").arg("-fsyntax-only").arg("-I").arg(linux().join(AMD).join("include")).arg(&file).output().unwrap_or_else(|e| panic!("no C compiler to check the offsets with (cc: {e})"))
}

#[test]
fn the_frames_and_mqds_offsets_are_the_c_compilers() {
    let c = source();
    let out = compile("mes12_offsets.c", &c);
    assert!(out.status.success(), "Linux's headers place a field elsewhere:\n{}", String::from_utf8_lossy(&out.stderr));
    // The check checks: a wrong number is refused by the same compiler.
    let right = format!("offsetof(union MESAPI__ADD_QUEUE, api_status) == {} ", at::ADD_STATUS * 4);
    assert!(c.contains(&right));
    let wrong = c.replace(&right, &format!("offsetof(union MESAPI__ADD_QUEUE, api_status) == {} ", (at::ADD_STATUS - 1) * 4));
    let out = compile("mes12_offsets_wrong.c", &wrong);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("api_status is not at dword"), "a wrong offset passed the compiler");
}

/// The flag words: the compiler's layout of the bitfields Linux sets, read back by a program the compiler builds
/// (bitfield order is the ABI's, so it is the compiler that must say it).
#[test]
fn the_flag_words_are_the_compilers_bitfields() {
    let mut c = source();
    c.push_str(&format!(
        "#include <stdio.h>\nint main(void) {{ printf(\"%#x %#x %#x %#x\\n\", hw.max_dwords_in_api[{}], add.max_dwords_in_api[{}], unmap.max_dwords_in_api[{}], preempt.max_dwords_in_api[{}]); return 0; }}\n",
        at::HW_FLAGS,
        at::ADD_FLAGS,
        at::REMOVE_FLAGS,
        at::REMOVE_FLAGS
    ));
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let (file, exe) = (dir.join("mes12_flags.c"), dir.join("mes12_flags"));
    std::fs::write(&file, &c).unwrap();
    let built = Command::new("cc").arg("-I").arg(linux().join(AMD).join("include")).arg(&file).arg("-o").arg(&exe).output().unwrap_or_else(|e| panic!("no C compiler (cc: {e})"));
    assert!(built.status.success(), "{}", String::from_utf8_lossy(&built.stderr));
    let ran = Command::new(&exe).output().expect("the program the compiler built");
    let want = format!("{:#x} {:#x} {:#x} {:#x}\n", api::hwflag::LINUX, api::ADD_MAP_LEGACY_KQ, api::REMOVE_UNMAP_LEGACY | api::REMOVE_AFTER_RESET, api::REMOVE_PREEMPT_LEGACY_GFX);
    assert_eq!(String::from_utf8_lossy(&ran.stdout), want);
}
