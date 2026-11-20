//! Wave 43 (piece 9 beyond read-only): the stage `pmfw`, Linux's start of the power controller after the secure
//! processor loaded its firmware: `smu_hw_init` (../pm/swsmu/amdgpu_smu.c:1955-2021), the trace's section smu
//! (init.txt lines 893-1883, 125 messages). The sequence is `rdna4_core::smuseq`'s, fed with the PMFW's answers;
//! each message goes out as Linux's `smu_msg_v1_send_msg` does it (`smu14::send`); the stage's guard holds a
//! `smugate::Gate` that lets a mailbox write through only when it is that sequence's next bus access.
//!
//! Before any access the stage refuses when: this PMFW was started already (once per load of its firmware by
//! `fw`), a question of the look `smu` has no answer yet, the card's runtime database does not say SCPM (Linux
//! would send masks no trace of this card shows), or the host is one where Linux lets the PMFW switch the link's
//! speed (another computation of OverridePcieParameters, not built). Then the firmware flags (Linux's
//! `smu_v14_0_check_fw_status`, a read Linux's trace cannot show), then the messages.
//!
//! A stop names the message, its argument and the answer with Linux's error for it. What a stop leaves to do
//! (`after_stop`, outside the comparison with Linux's trace): the video block is powered down again where Linux's
//! `err_out` does it (amdgpu_smu.c:915-921; a PowerDownJpeg that failed is followed by PowerDownVcn all the same,
//! :912-913), and once EnableAllSmuFeatures went out the running features are read back and said. A PMFW that
//! did not answer, or answered a code Linux takes for a hang, gets nothing more, and neither does one behind a
//! bus that failed the stage (a register or the driver table that does not read, a write that did not arrive).
//! What a stop cannot take back is said: the thermal alert's registers, once written, stay as written (as on
//! Linux).

use super::attach::LinkHost;
use super::guard::Allow;
use super::linuxseq::rules::{Argument, Rule};
use super::regs14::Regs14;
use super::smu14::{self, Line, SmuStop};
use super::stage::{Ctx, Session, Stage, Stop, read, stop};
use super::vram::Layout;
use crate::bo::PAGE;
use crate::gpucore::psp::{self, Hw};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rdna4_core::pptable14::Pptable;
use rdna4_core::regmap::SMN_MP1_FIRMWARE_FLAGS;
use rdna4_core::smu14::{self as core14, DRIVER_TABLE_BYTES, TOOLS_TABLE_BYTES, msg};
use rdna4_core::smugate::Gate;
use rdna4_core::smuseq::{Script, Step, Stopped, Tables};
use rdna4_core::{pcielink, refuse, thm14};

/// What the stage may write: the index pair's address register (the firmware flags), the mailbox, and the thermal
/// alert's four registers. No memory.
pub fn allow(r: &Regs14) -> Allow {
    Allow { stage: Stage::Pmfw.name(), regs: alloc::vec![r.rsmu_index, r.smu_answer, r.smu_argument, r.smu_message, r.thm_int_ctrl, r.thm_int_ena, r.mp1_ih_sw_int, r.mp1_ih_sw_int_ctrl], mem: Vec::new() }
}

/// Where this stage differs from Linux's trace (section smu) and says so. `placed`: where the VRAM manager's
/// record has the two tables (`pmfw::placed`); the stage's arguments must name exactly those places.
pub fn linux_rules(l: &Layout, placed: Tables) -> [Rule; 3] {
    let vram = l.mc(0)..l.mc(l.vram_bytes);
    let table = |hi, lo, at, bytes, why| Rule::Argument(Argument { arg: "MP1_SMN_C2PMSG_82", msg: "MP1_SMN_C2PMSG_66", hi: u32::from(hi), lo: u32::from(lo), at, align: PAGE, within: vram.clone(), bytes, why });
    [
        Rule::Untraced {
            first: "MP1_SMN_C2PMSG_90",
            byte: SMN_MP1_FIRMWARE_FLAGS,
            why: "Linux reads the SMU's firmware flags here through RREG32_PCIE, which has no tracepoint (smu_v14_0.c:213-227, amdgpu_reg_access.c:194-201)",
        },
        table(msg::SET_DRIVER_DRAM_ADDR_HIGH, msg::SET_DRIVER_DRAM_ADDR_LOW, placed.driver_mc, DRIVER_TABLE_BYTES, "the driver table lies where BitsOS's VRAM manager put it; Linux's lay at MC 0x83daaf8000"),
        table(msg::SET_TOOLS_DRAM_ADDR_HIGH, msg::SET_TOOLS_DRAM_ADDR_LOW, placed.tools_mc, TOOLS_TABLE_BYTES, "the tools table lies where BitsOS's VRAM manager put it; Linux's lay at MC 0x83daac0000"),
    ]
}

/// What the stage stands on, known before any access.
struct Plan {
    tables: Tables,
    caps: (u8, u8),
}

/// The stage's own checks, and its two tables (taken from the VRAM manager at the first call of a boot).
fn ready(s: &mut Session, c: &Ctx) -> Result<Plan, Stop> {
    if s.pmfw.began {
        return Err(stop("die Energieverwaltung wurde seit ihrem Laden schon einmal gestartet", "pmfw ran on this PMFW (to its end, or it stopped after its first message): once per load of its firmware by the stage fw"));
    }
    if s.pmfw.owes_answer {
        return Err(stop(
            "Energieverwaltung ist noch mit einer alten Frage beschäftigt",
            "a question of the look smu has no answer yet; Linux's first message does not look at the answer register (smu_cmn.c:508-511) and would be written over a message that still runs",
        ));
    }
    if !c.att.scpm.enabled() {
        return Err(stop(
            "die Karte verwaltet ihre Energietabelle anders, als Linux' Mitschrift es zeigt",
            format!("the PSP's runtime database says {:?} for SCPM: Linux would then send SetAllowedFeaturesMaskHigh/Low (amdgpu_smu.c:1806-1812), which the trace of this card does not hold and BitsOS does not send", c.att.scpm),
        ));
    }
    if c.facts.link_host != LinkHost::Fixed {
        return Err(stop(
            "auf diesem Rechner stellt Linux die Verbindung der Karte anders ein, das ist nicht gebaut",
            "the host is one where Linux lets the PMFW switch the link's speed (amdgpu_device.c:1298-1309): OverridePcieParameters is computed another way there, which no trace of this card shows",
        ));
    }
    let (Some(l), Some(bump)) = (s.layout.as_ref(), s.bump.as_mut()) else {
        return Err(stop("zuerst den Speicher der Karte einrichten", "no VRAM layout in this boot: the stage gmc plans it"));
    };
    if s.pmfw.tables.is_none() {
        let room = |e| stop("kein Platz im Speicher der Karte", format!("{e:?}"));
        // Linux's order (the trace's buffer events, init.txt lines 276 and 278): the tools table, then the driver table.
        let tools = bump.take(super::pmfw::TOOLS_TABLE, TOOLS_TABLE_BYTES, PAGE).map_err(room)?;
        let driver = match bump.take(super::pmfw::DRIVER_TABLE, DRIVER_TABLE_BYTES, PAGE) {
            Ok(d) => d,
            Err(e) => {
                bump.give_back(&tools);
                return Err(room(e));
            }
        };
        s.pmfw.tables = Some((tools, driver));
    }
    let Some((tools, driver)) = s.pmfw.tables.as_ref() else { return Err(stop("kein Platz im Speicher der Karte", "no tables")) };
    Ok(Plan { tables: Tables { driver_mc: l.mc(driver.start), tools_mc: l.mc(tools.start) }, caps: pcielink::caps(&c.facts.path) })
}

/// The gate the stage's guard holds: its own copy of Linux's sequence, moved by the bus alone. `None` when the
/// stage will refuse before any access.
pub(super) fn gate(s: &mut Session, c: &Ctx) -> Option<alloc::boxed::Box<Gate>> {
    ready(s, c).ok().map(|p| alloc::boxed::Box::new(Gate::new(Script::new(p.tables, p.caps))))
}

fn name(m: u16) -> &'static str {
    refuse::SMU_START.iter().chain(refuse::SMU_QUESTIONS.iter()).find(|(x, _)| *x == m).map_or("?", |(_, n)| n)
}

fn smu_plain(e: &SmuStop) -> &'static str {
    match e {
        SmuStop::Gone { .. } => "Karte antwortet beim Lesen nicht",
        SmuStop::Refused { .. } => "ein Schreibzugriff war nicht erlaubt",
        SmuStop::NotRunning { .. } => "Energieverwaltung der Karte läuft nicht",
        SmuStop::Busy { .. } => "Energieverwaltung ist noch mit dem Befehl davor beschäftigt",
        SmuStop::Hung { .. } => "Energieverwaltung steckt in einem Fehlerzustand und bekommt keinen Befehl mehr",
        SmuStop::Timeout { .. } => "Energieverwaltung antwortet nicht",
        SmuStop::Answer { .. } => "Energieverwaltung lehnt einen Befehl ab",
    }
}

/// One message of the sequence; the script is told what became of it.
fn message(hw: &mut dyn Hw, r: &Regs14, script: &mut Script, line: &mut Line, m: u16, arg: u32, value: bool) -> Result<Option<u32>, Stop> {
    let at = format!("{} ({m:#04x}, argument {arg:#x})", name(m));
    match smu14::send(hw, r, line, m, arg, value) {
        Ok(v) => {
            script.sent();
            script.answered(v).map_err(|why| stop("Energieverwaltung antwortet anders, als Linux' Ablauf es verträgt", format!("after {at}: {why:?}")))?;
            Ok(v)
        }
        Err(e) => {
            let plain = smu_plain(&e);
            let detail = match e {
                SmuStop::Answer { code, .. } => {
                    script.sent();
                    script.failed(code);
                    format!("{at} answered {code:#x}: Linux's {} (smu_cmn.c:249-292)", core14::decode(code).errno())
                }
                SmuStop::Timeout { .. } => {
                    script.sent();
                    script.failed(0);
                    format!("{at}: no answer within Linux's {} reads of the answer register (-ETIME); nothing more is sent", smu14::ANSWER_READS)
                }
                SmuStop::Busy { .. } | SmuStop::Hung { .. } => {
                    script.mute();
                    format!("before {at}: {e:?}; Linux sends nothing then (smu_cmn.c:514-522), neither does this")
                }
                _ => {
                    script.mute();
                    format!("at {at}: {e:?}; nothing more is sent")
                }
            };
            Err(stop(plain, detail))
        }
    }
}

/// `smu_v14_0_set_irq_state` with AMDGPU_IRQ_STATE_ENABLE (smu_v14_0.c:754-797), register for register.
/// `wrote` counts the registers that were written: a stop after the first leaves them as written.
fn thermal(hw: &mut dyn Hw, r: &Regs14, t: &Pptable, wrote: &mut u8, out: &mut Vec<String>) -> Result<(), Stop> {
    let mut put = |hw: &mut dyn Hw, byte, v| {
        psp::write(hw, byte, v).map_err(super::stage::psp_stop)?;
        *wrote += 1;
        Ok::<(), Stop>(())
    };
    let was = read(hw, r.thm_int_ctrl)?;
    let now = thm14::int_ctrl(was, t.software_shutdown_temp, core14::THERMAL_MIN_MILLI);
    put(hw, r.thm_int_ctrl, now)?;
    put(hw, r.thm_int_ena, thm14::INT_ENA)?;
    let sw = read(hw, r.mp1_ih_sw_int)?;
    put(hw, r.mp1_ih_sw_int, thm14::sw_int(sw))?;
    let ctrl = read(hw, r.mp1_ih_sw_int_ctrl)?;
    put(hw, r.mp1_ih_sw_int_ctrl, thm14::sw_int_ctrl(ctrl))?;
    out.push(format!("thermal alert: THM_THERMAL_INT_CTRL {was:#010x} -> {now:#010x} (upper threshold {} degrees, the table's software shutdown temperature), MP1's interrupt id 0xfe unmasked; the IH reader hands both to the thermal taker (Linux's handler)", now & thm14::DIG_THERM_INTH));
    Ok(())
}

/// The sequence from where it stands to its end or its first stop.
fn drive(s: &mut Session, hw: &mut dyn Hw, r: &Regs14, script: &mut Script, line: &mut Line, out: &mut Vec<String>) -> Result<(), Stop> {
    loop {
        match script.next() {
            Step::End => return Ok(()),
            Step::Send { msg: m, arg, value } => {
                // Before the write: a call that ends in the middle has said that it began.
                s.pmfw.began = true;
                let v = message(hw, r, script, line, m, arg, value)?;
                match (m, v) {
                    (msg::GET_RUNNING_SMU_FEATURES_HIGH, _) => {}
                    (msg::GET_RUNNING_SMU_FEATURES_LOW, _) => out.push(format!("smu running features {}", script.facts.features.map_or(String::from("?"), |f| format!("{f:#018x}")))),
                    (_, Some(v)) => out.push(format!("smu {} ({m:#04x}, argument {arg:#x}): OK, value {v:#010x}", name(m))),
                    (_, None) => out.push(format!("smu {} ({m:#04x}, argument {arg:#x}): OK", name(m))),
                }
            }
            Step::Table => {
                // On the heap: 16 KiB (a stage runs on a small stack).
                let mut bytes = alloc::vec![0u8; DRIVER_TABLE_BYTES as usize];
                let at = script.tables().driver_mc;
                if !hw.mem_read(at, &mut bytes) {
                    script.mute();
                    return Err(stop("Speicher der Karte lässt sich nicht lesen", format!("the driver table at mc {at:#x} does not read")));
                }
                script.table(&bytes).map_err(|why| stop("die Tabelle der Energieverwaltung ist nicht die, die Linux' Vorlage beschreibt", format!("the PMFW's table at mc {at:#x}: {why:?}; nothing after TransferTableSmu2Dram was sent")))?;
                if let Some(t) = script.facts.table {
                    table_lines(&t, script, out);
                }
            }
            Step::Thermal => {
                let Some(t) = script.facts.table else {
                    script.mute();
                    return Err(stop("unerwarteter Fehler im Ablauf", "no table at the thermal alert"));
                };
                if let Err(e) = thermal(hw, r, &t, &mut script.facts.alert_written, out) {
                    // The bus failed the stage: nothing more goes out, the read-back of what runs neither.
                    script.mute();
                    return Err(e);
                }
                script.thermal_done();
            }
        }
    }
}

fn table_lines(t: &Pptable, script: &Script, out: &mut Vec<String>) {
    out.push(format!(
        "pmfw table: {} bytes, format revision {} (Linux's header: {}), content revision {}; platform caps {:#x}, software shutdown temperature {} degrees, fan control {}, GameClockAc {} MHz, socket power limit {} W (message limit {} W)",
        t.structuresize,
        t.format_revision,
        rdna4_core::pptable14::TABLE_FORMAT_REVISION,
        t.content_revision,
        t.platform_caps,
        t.software_shutdown_temp,
        if t.no_fan() { "not asked for by the table" } else { "asked for by the table" },
        t.game_clock_ac,
        t.socket_power_limit_ac,
        t.msg_limit_power_ac
    ));
    out.push(format!("pmfw table's own header places its parts at {:?}; Linux reads them by its compiled header at {:?} (it never reads the table's own offsets)", t.own, Pptable::COMPILED));
    let (args, n) = script.facts.pcie;
    out.push(format!("link levels of the table: generation codes {:?}, width codes {:?}, LCLK {:?}; OverridePcieParameters arguments {:x?}", t.pcie.gen_speed, t.pcie.lane_count, t.pcie.lclk_freq, &args[..n]));
}

fn summary(script: &Script, out: &mut Vec<String>) {
    let f = &script.facts;
    if let (Some(i), Some(v)) = (f.fw_if, f.version) {
        out.push(format!("smu version {}.{}.{} (program {}), driver interface {i:#x} (Linux 7.3.1 is built for {:#x} and goes on)", v >> 16 & 0xff, v >> 8 & 0xff, v & 0xff, v >> 24, smu14::DRIVER_IF));
    }
    if let Some(m) = f.supported {
        out.push(format!("features on after EnableAllSmuFeatures: {m:#018x}; dpm running {:?}; fan control {}", f.dpm_running, if m & (1 << core14::feature::FAN_CONTROL) != 0 { "on (the PMFW's own curve, zero RPM at idle)" } else { "off" }));
    }
    for t in &f.dpm {
        out.push(match t.from_pmfw {
            true => format!("clock {}: {} levels {:?} MHz{}", t.clock, t.mhz.len(), t.mhz, if t.fine_grained { ", any frequency between the first and the last" } else { "" }),
            false => format!("clock {}: its feature does not run, no table asked (Linux would keep the VBIOS's boot value, which BitsOS does not read)", t.clock),
        });
    }
    if let Some(t) = f.thermal {
        out.push(format!("thermal range: edge {} C, hotspot {} C, memory {} C (emergency +{} C each), software shutdown {} C", t.max / 1000, t.hotspot_crit_max / 1000, t.mem_crit_max / 1000, core14::CTF_OFFSET, t.software_shutdown_temp));
    }
}

/// The stage.
pub(super) fn pmfw(s: &mut Session, c: &Ctx, hw: &mut dyn Hw, r: &Regs14, out: &mut Vec<String>) -> Result<(), Stop> {
    let plan = ready(s, c)?;
    let (gen_cap, width_cap) = plan.caps;
    out.push(format!("tables in vram: driver table mc {:#x} ({DRIVER_TABLE_BYTES:#x} bytes), tools table mc {:#x} ({TOOLS_TABLE_BYTES:#x} bytes)", plan.tables.driver_mc, plan.tables.tools_mc));
    out.push(format!("scpm: {:?} (read at the attach); link path {:?}: Linux's limit is generation code {gen_cap}, width code {width_cap}", c.att.scpm, c.facts.path));
    let mut script = Script::new(plan.tables, plan.caps);
    // `smu_start_smc_engine` (amdgpu_smu.c:1937-1942): a firmware that does not run gets no message.
    let flags = smu14::check_fw_status(hw, r).map_err(|e| stop(smu_plain(&e), format!("{e:?}")))?;
    out.push(format!("smu: firmware flags {flags:#010x} at SMN {SMN_MP1_FIRMWARE_FLAGS:#x}"));
    // Every `smu_hw_init` sets SMU_FW_INIT: the first message does not look at the answer register.
    let mut line = Line { first: true, ..Line::default() };
    let ran = drive(s, hw, r, &mut script, &mut line, out);
    s.smu_asked |= line.sent;
    summary(&script, out);
    s.pmfw.facts = script.facts.clone();
    s.pmfw.stopped = script.stopped();
    super::thermal::arm(s, r, ran.is_ok() && script.passed());
    match ran {
        Ok(()) if script.passed() => {
            s.pmfw.passed = true;
            klog::event!(Info, "gx", "rdna4.pmfw.started", features = klog::Hex(s.pmfw.facts.supported.unwrap_or(0)), version = klog::Hex(u64::from(s.pmfw.facts.version.unwrap_or(0))); "the power controller's features are on, the graphics block has power");
            out.push(String::from("the graphics block has power: EnableAllSmuFeatures was answered, the clocks' tables are kept for the stages after this one"));
            Ok(())
        }
        Ok(()) => Err(stop("unerwarteter Fehler im Ablauf", "the sequence ended before Linux's last message")),
        Err(e) => {
            s.pmfw.run = Some(alloc::boxed::Box::new((script, line)));
            Err(e)
        }
    }
}

/// What a stop leaves to do, with the same mailbox and behind the same guard; not part of Linux's cold start, so
/// `stage::run` keeps these accesses out of the comparison.
pub(super) fn after_stop(s: &mut Session, hw: &mut dyn Hw, r: &Regs14, out: &mut Vec<String>) {
    let Some(run) = s.pmfw.run.take() else { return };
    let (mut script, mut line) = *run;
    let (mut down, mut why) = (Vec::new(), None);
    while let Step::Send { msg: m, arg, value } = script.next() {
        match message(hw, r, &mut script, &mut line, m, arg, value) {
            Ok(_) if !value => down.push(name(m)),
            Ok(_) => {}
            Err(Stop(_, detail)) => why = Some(detail),
        }
    }
    let f = &script.facts;
    if !down.is_empty() {
        out.push(format!("after the stop: {} answered OK (Linux's err_out, amdgpu_smu.c:915-921)", down.join(", ")));
    }
    if f.vcn_up || f.jpeg_up {
        out.push(format!("after the stop: the video block stays powered up (VCN {}, JPEG {}): a power-down that was not answered OK is not sent a second time, and a PMFW that takes no message gets none", f.vcn_up, f.jpeg_up));
    }
    if f.alert_written > 0 {
        out.push(format!(
            "after the stop: {} of the thermal alert's four registers were written (THM_THERMAL_INT_CTRL, THM_THERMAL_INT_ENA, MP1_SMN_IH_SW_INT, MP1_SMN_IH_SW_INT_CTRL, in that order) and stay as written: Linux's smu_smc_hw_setup returns without taking them back too (amdgpu_smu.c:1885-1889); with all four the alert is armed, and the thermal taker at the IH reader handles what it raises",
            f.alert_written
        ));
    }
    out.push(match (f.enable_sent, f.read_back) {
        (false, _) => String::from("after the stop: EnableAllSmuFeatures did not go out; the PMFW's features are as its own start left them"),
        (true, Some(m)) => format!("after the stop: EnableAllSmuFeatures went out; running features read back {m:#018x} (graphics block {})", if f.enabled { "powered" } else { "not known to be powered: the message was not answered OK" }),
        (true, None) => format!(
            "after the stop: EnableAllSmuFeatures went out; the running features were not read back ({})",
            why.unwrap_or_else(|| String::from(if script.stopped() == Some(Stopped::Stage) { "nothing is sent after an answer register that did not read as answered, or after the bus failed the stage" } else { "the PMFW takes no message" }))
        ),
    });
    s.pmfw.facts = script.facts;
}
