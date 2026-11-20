//! `rdna4_core::pptable14`'s offsets against Linux 7.3.1's own headers: a C file that includes them and asserts
//! every offset and size at compile time (`_Static_assert` over `offsetof`), checked by the host's C compiler
//! with `-fsyntax-only` (nothing is built or run). Linux reads the PMFW's table by exactly these compiled
//! offsets, so a number here that is not the compiler's would read another field.
//!
//! The tree is the desk's read-only copy: `$LINUX_REF`, else /var/tmp/claude-v2/ref/linux-7.3.1 (as
//! host/gen/rdna4), else `$HOME/BitsOS/V2/ref/linux-7.3.1`. Without a tree or a C compiler the test fails and
//! says so: it never passes unchecked.

use rdna4_core::pptable14 as p;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;

const AMD: &str = "drivers/gpu/drm/amd";

fn linux() -> PathBuf {
    let mut tried = Vec::new();
    let home = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("BitsOS/V2/ref/linux-7.3.1"));
    for dir in [std::env::var_os("LINUX_REF").map(PathBuf::from), Some(PathBuf::from("/var/tmp/claude-v2/ref/linux-7.3.1")), home].into_iter().flatten() {
        if dir.join(AMD).join("pm/swsmu/inc/smu_v14_0_2_pptable.h").exists() {
            return dir;
        }
        tried.push(dir.display().to_string());
    }
    panic!("no Linux 7.3.1 tree with {AMD}/pm/swsmu/inc/smu_v14_0_2_pptable.h (tried {tried:?}; set LINUX_REF to one)");
}

#[test]
fn the_tables_offsets_are_the_c_compilers() {
    let outer = "struct smu_14_0_2_powerplay_table";
    let mut c = String::from(
        "#include <stdint.h>\n#include <stddef.h>\n#include <stdbool.h>\n\
         /* include/atomfirmware.h:233-238 */\n\
         struct atom_common_table_header { uint16_t structuresize; uint8_t format_revision; uint8_t content_revision; };\n\
         /* The order of pm/swsmu/smu14/smu_v14_0_2_ppt.c:35-39: the driver interface, then the packed table. */\n\
         #include \"pm/swsmu/inc/pmfw_if/smu14_driver_if_v14_0.h\"\n\
         #include \"pm/swsmu/inc/smu_v14_0_2_pptable.h\"\n",
    );
    let mut off = |ty: &str, field: &str, want: usize| writeln!(c, "_Static_assert(offsetof({ty}, {field}) == {want}, \"{ty}.{field} is not at {want}\");").unwrap();
    off(outer, "header.structuresize", p::STRUCTURESIZE);
    off(outer, "header.format_revision", p::FORMAT_REVISION);
    off(outer, "header.content_revision", p::CONTENT_REVISION);
    off(outer, "pmfw_pptable_start_offset", p::PMFW_PARTS);
    off(outer, "pmfw_custom_sku_table_size", p::PMFW_PARTS + 14);
    off(outer, "platform_caps", p::PLATFORM_CAPS);
    off(outer, "thermal_controller_type", p::THERMAL_CONTROLLER_TYPE);
    off(outer, "software_shutdown_temp", p::SOFTWARE_SHUTDOWN_TEMP);
    off(outer, "smc_pptable", p::SMC_PPTABLE);
    off("PPTable_t", "PFE_Settings", 0);
    off("PPTable_t", "SkuTable", p::SKU_TABLE);
    off("PPTable_t", "CustomSkuTable", p::CUSTOM_SKU_TABLE);
    off("PPTable_t", "BoardTable", p::BOARD_TABLE);
    off("PFE_Settings_t", "FeaturesToRun", p::FEATURES_TO_RUN);
    off("SkuTable_t", "PcieGenSpeed", p::PCIE_GEN_SPEED);
    off("SkuTable_t", "PcieLaneCount", p::PCIE_LANE_COUNT);
    off("SkuTable_t", "LclkFreq", p::LCLK_FREQ);
    off("SkuTable_t", "DriverReportedClocks", p::DRIVER_REPORTED_CLOCKS);
    off("DriverReportedClocks_t", "GameClockAc", p::GAME_CLOCK_AC);
    off("SkuTable_t", "MsgLimits", p::MSG_LIMITS);
    off("MsgLimits_t", "Power", 0);
    off("CustomSkuTable_t", "SocketPowerLimitAc", p::SOCKET_POWER_LIMIT_AC);
    off("CustomSkuTable_t", "TemperatureLimit", p::TEMPERATURE_LIMIT);
    off("CustomSkuTable_t", "FanAbnormalTempLimitOffset", p::FAN_ABNORMAL_TEMP_LIMIT_OFFSET);
    let mut is = |expr: &str, want: usize| writeln!(c, "_Static_assert(({expr}) == {want}, \"{expr} is not {want}\");").unwrap();
    is(&format!("sizeof({outer})"), p::TABLE_BYTES);
    is("sizeof(PPTable_t)", p::PPTABLE_BYTES);
    is("sizeof(SkuTable_t)", p::SKU_TABLE_BYTES);
    is("sizeof(CustomSkuTable_t)", p::CUSTOM_SKU_TABLE_BYTES);
    is("sizeof(BoardTable_t)", p::BOARD_TABLE_BYTES);
    is("NUM_LINK_LEVELS", p::LINK_LEVELS);
    is("TEMP_EDGE", p::TEMP_EDGE);
    is("TEMP_HOTSPOT", p::TEMP_HOTSPOT);
    is("TEMP_MEM", p::TEMP_MEM);
    is("SMU_14_0_2_TABLE_FORMAT_REVISION", usize::from(p::TABLE_FORMAT_REVISION));
    is("PPT_THROTTLER_PPT0", 0);
    is("FEATURE_FAN_CONTROL_BIT", usize::from(rdna4_core::smu14::feature::FAN_CONTROL));
    is("TABLE_COMBO_PPTABLE", rdna4_core::smu14::TABLE_COMBO_PPTABLE as usize);
    // The field types the reader assumes (sizes of one element).
    is("sizeof(((SkuTable_t *)0)->PcieGenSpeed[0])", 1);
    is("sizeof(((SkuTable_t *)0)->LclkFreq[0])", 2);
    is(&format!("sizeof((({outer} *)0)->software_shutdown_temp)"), 2);
    is("sizeof(((CustomSkuTable_t *)0)->TemperatureLimit[0])", 2);
    is("sizeof(((PFE_Settings_t *)0)->FeaturesToRun)", 8);

    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let file = dir.join("pptable_offsets.c");
    std::fs::write(&file, &c).unwrap();
    let out = Command::new("cc").arg("-fsyntax-only").arg("-I").arg(linux().join(AMD)).arg(&file).output().unwrap_or_else(|e| panic!("no C compiler to check the offsets with (cc: {e})"));
    assert!(out.status.success(), "Linux's headers place a field elsewhere:\n{}", String::from_utf8_lossy(&out.stderr));
    // The check checks: a wrong number is refused by the same compiler.
    let wrong = c.replace(&format!("offsetof({outer}, smc_pptable) == {}", p::SMC_PPTABLE), &format!("offsetof({outer}, smc_pptable) == {}", p::SMC_PPTABLE + 1));
    assert_ne!(wrong, c);
    let file = dir.join("pptable_offsets_wrong.c");
    std::fs::write(&file, &wrong).unwrap();
    let out = Command::new("cc").arg("-fsyntax-only").arg("-I").arg(linux().join(AMD)).arg(&file).output().unwrap();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("smc_pptable is not at"), "a wrong offset passed the compiler");
}

/// Wave 44: the metrics table Linux reads a temperature from (`rdna4_core::metrics14`), held to the same headers.
#[test]
fn the_metrics_offsets_are_the_c_compilers() {
    use rdna4_core::metrics14 as m;
    let mut c = String::from("#include <stdint.h>\n#include <stddef.h>\n#include <stdbool.h>\n#include \"pm/swsmu/inc/pmfw_if/smu14_driver_if_v14_0.h\"\n");
    let mut is = |expr: &str, want: usize| writeln!(c, "_Static_assert(({expr}) == {want}, \"{expr} is not {want}\");").unwrap();
    is("sizeof(SmuMetricsExternal_t)", m::METRICS_BYTES);
    is("sizeof(SmuMetrics_t)", m::SMU_METRICS_BYTES);
    is("offsetof(SmuMetricsExternal_t, SmuMetrics)", 0);
    is("offsetof(SmuMetrics_t, MetricsCounter)", m::METRICS_COUNTER);
    is("offsetof(SmuMetrics_t, AvgTemperature)", m::AVG_TEMPERATURE);
    is("offsetof(SmuMetrics_t, AvgTemperatureFanIntake)", m::AVG_TEMPERATURE_FAN_INTAKE);
    is("sizeof(((SmuMetrics_t *)0)->AvgTemperature[0])", 2);
    is("sizeof(((SmuMetrics_t *)0)->MetricsCounter)", 4);
    is("TEMP_COUNT", m::TEMP_COUNT);
    is("TABLE_SMU_METRICS", m::TABLE_SMU_METRICS as usize);
    is("TEMP_EDGE", p::TEMP_EDGE);
    is("TEMP_HOTSPOT", p::TEMP_HOTSPOT);
    is("TEMP_MEM", p::TEMP_MEM);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let file = dir.join("metrics_offsets.c");
    std::fs::write(&file, &c).unwrap();
    let out = Command::new("cc").arg("-fsyntax-only").arg("-I").arg(linux().join(AMD)).arg(&file).output().unwrap_or_else(|e| panic!("no C compiler to check the offsets with (cc: {e})"));
    assert!(out.status.success(), "Linux's headers place a metrics field elsewhere:\n{}", String::from_utf8_lossy(&out.stderr));
    let wrong = c.replace(&format!("AvgTemperature)) == {}", m::AVG_TEMPERATURE), &format!("AvgTemperature)) == {}", m::AVG_TEMPERATURE + 2));
    assert_ne!(wrong, c);
    let file = dir.join("metrics_offsets_wrong.c");
    std::fs::write(&file, &wrong).unwrap();
    let out = Command::new("cc").arg("-fsyntax-only").arg("-I").arg(linux().join(AMD)).arg(&file).output().unwrap();
    assert!(!out.status.success(), "a wrong metrics offset passed the compiler");
}
