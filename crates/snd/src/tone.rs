//! GP-SOUND: the tone check's doors and the report page's sound line. `tone_check` is the only thing that makes a sound
//! without a program asking (one quiet second, `usb_audio::check`); the console's `snd ton` and the MCP tool `snd_ton`
//! call it, nothing at boot does. It is refused without the test key `ktest_georg=on` (usb-audio's `check::allowed`) and
//! while anything is open on the output, since two streams on one endpoint would interleave.

use crate::backend::{Endpoint, Port, Target};
use crate::output::{self, current_target};
use crate::{hdaback, usbback};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult, mcp_tool};
use usb_audio::OutputInfo;

/// One quiet second of 750 Hz on a USB output (none: the first, which `auto` plays on); one German line back.
pub fn tone_check(name: Option<&str>) -> String {
    usb_audio::check::tone(name, &|ep| crate::started().and_then(|m| m.status().endpoint).as_deref() == Some(ep))
}

fn target_word(t: &Target) -> &'static str {
    match t {
        Target::Auto => "automatisch",
        Target::Prefer(Port::Jack) => "Buchse",
        Target::Prefer(Port::Hdmi) => "HDMI",
        Target::Prefer(Port::Usb) => "USB",
        Target::Fixed { .. } => "fest",
    }
}

/// hda's playable outputs by kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HdaCount {
    pub jacks: usize,
    pub hdmi: usize,
}

/// The port in an output's name (`Endpoint::name`: dev/port/cad:pin).
fn port_of(name: &str) -> Option<Port> {
    match name.rsplit('/').nth(1)? {
        "jack" => Some(Port::Jack),
        "hdmi" => Some(Port::Hdmi),
        "usb" => Some(Port::Usb),
        _ => None,
    }
}

/// What the line calls the output named `name`.
fn named(name: &str, usb: &[OutputInfo]) -> String {
    match port_of(name) {
        Some(Port::Jack) => String::from("die Buchse am Rechner"),
        Some(Port::Hdmi) => String::from("den Bildschirm (HDMI/DP)"),
        _ => usb.iter().find(|o| usb_audio::check::endpoint_name(o) == name).map(usb_audio::check::label).unwrap_or_else(|| String::from("einen USB-Ausgang")),
    }
}

/// The sound line from its facts: `playing` is the mixer's output now, `would` where the target leads while it is
/// still; `tone`: the tone check may run here.
pub fn sound_line(t: &Target, playing: Option<&str>, would: Option<&Endpoint>, usb: &[OutputInfo], hda: HdaCount, muted: bool, refused: &[String], tone: bool)
    -> String
{
    let now = match (playing, would) {
        (Some(p), _) => format!("spielt auf {}", named(p, usb)),
        (None, Some(e)) => format!("still, ginge auf {}", named(&e.name(), usb)),
        (None, None) => String::from("still, kein Ausgang da"),
    };
    let list = if usb.is_empty() { String::from("keine") } else { usb_audio::check::listing(usb) };
    let mut line = format!("Ton: {now} ({}); USB-Ausgänge: {list}; Buchsen am Rechner: {}; Bildschirm-Ausgänge: {}", target_word(t), hda.jacks, hda.hdmi);
    if muted {
        line.push_str("; Hauptschalter stumm");
    }
    for r in refused {
        line.push_str(&format!("; nicht bereit: {r}"));
    }
    if tone {
        line.push_str("; Probe: snd ton");
    }
    line
}

/// The report page's line for "Ton" (GP-ROUND's getter): where sound goes now (or would go), the USB outputs in the
/// order `snd ton <number>` counts them, hda's outputs by kind, the jacks that did not come up and why, and the
/// command that tests one where it may run.
pub fn report_line() -> String {
    let outs = usb_audio::outputs();
    let t = current_target();
    let playing = crate::started().and_then(|m| m.status().endpoint);
    let would = usbback::pair_resolve(&t, &mut |x| usbback::usb_resolve(&outs, x), &mut |x| hdaback::resolve_now(x));
    let mut hda = HdaCount::default();
    for d in hda::controllers() {
        for o in d.ctrl.outputs().iter().filter(|o| o.playable) {
            if o.port == devmodel::audio::Output::Hdmi {
                hda.hdmi += 1;
            } else {
                hda.jacks += 1;
            }
        }
    }
    let tone = usb_audio::check::allowed_here().is_ok();
    sound_line(&t, playing.as_deref(), would.as_ref(), &outs, hda, usb_audio::hw_mute(), &usb_audio::uac2dev::refused(), tone)
}

fn snd_ton(_ctx: &ToolContext, args: &Json) -> ToolResult {
    let name = match args.get("output") {
        None | Some(Json::Null) => None,
        Some(Json::Str(v)) => Some(v.clone()),
        Some(_) => return Err(ToolError::BadArgument(String::from("output must be text: a number from the list, headset, lautsprecher, kopfhoerer, spdif or a full name"))),
    };
    let line = tone_check(name.as_deref());
    let mut out: Vec<String> = vec![line];
    out.extend(output::status_lines());
    Ok(vec![ToolOutput::Text(out.join("\n"))])
}

mcp_tool!(
    name: "snd_ton",
    owner: "SN",
    safety: Change,
    description: "Makes a sound: one quiet second (750 Hz, about -30 dB below full scale) on one USB sound output, and answers in one German line what the device said at each step (found, rate set and clock valid, stream started, bytes sent, stopped), then the snd status. Only for the hardware day with a person listening; nothing else in BitsOS plays a tone unasked. Refused unless CONFIG has the test key ktest_georg=on (Georg listens), and while anything is open on that output. A muted master switch is named in the line. output: a number of the list in the report page's sound line, headset, lautsprecher, kopfhoerer, spdif or the full name the snd tool shows; none: the first, where auto plays.",
    schema: r#"{"type":"object","properties":{"output":{"type":"string","description":"which output: a number, headset, lautsprecher, kopfhoerer, spdif or a full name; none: the default"}}}"#,
    handler: snd_ton,
);
