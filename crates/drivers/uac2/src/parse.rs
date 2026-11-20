//! The walk over a configuration descriptor as `GET_DESCRIPTOR` returns it. A broken tree is refused as a whole: half
//! of one would route sound to the wrong jack.

use crate::desc::{ADC_2_00, AltSetting, ClockSource, DescError, Endpoint, FeatureUnit, Function, InputTerminal};
use crate::desc::{OutputTerminal, PROTOCOL_UAC2};

const DT_CONFIGURATION: u8 = 2;
const DT_INTERFACE: u8 = 4;
const DT_ENDPOINT: u8 = 5;
const DT_CS_INTERFACE: u8 = 0x24;
const CLASS_AUDIO: u8 = 1;
const SUBCLASS_CONTROL: u8 = 1;
const SUBCLASS_STREAMING: u8 = 2;
mod ac {
    pub const HEADER: u8 = 1;
    pub const INPUT_TERMINAL: u8 = 2;
    pub const OUTPUT_TERMINAL: u8 = 3;
    pub const FEATURE_UNIT: u8 = 6;
    pub const CLOCK_SOURCE: u8 = 0x0A; // V:255
}

mod as_ {
    pub const GENERAL: u8 = 1;
    pub const FORMAT_TYPE: u8 = 2;
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

#[derive(Clone, Copy)]
enum Current {
    Other,
    Control,
    Streaming { iface: u8, alt: u8 },
}

#[derive(Default)]
struct PartialAlt {
    general: Option<(u8, u8, u32, u8, u32)>,
    format: Option<(u8, u8)>,
    endpoint: Option<Endpoint>,
    feedback: Option<u8>,
}

fn finish(function: &mut Function, current: Current, partial: PartialAlt) -> Result<(), DescError> {
    let Current::Streaming { iface, alt } = current else {
        return Ok(());
    };
    // Alternate setting 0 of a streaming interface is the idle one, with nothing in it.
    if partial.general.is_none() && partial.format.is_none() && partial.endpoint.is_none() {
        return Ok(());
    }
    let incomplete = DescError::IncompleteAlt { iface, alt };
    let (link, format_type, formats, channels, channel_config) = partial.general.ok_or(incomplete)?;
    let (subslot, bits) = partial.format.ok_or(incomplete)?;
    let endpoint = partial.endpoint.ok_or(incomplete)?;
    function.alts.push(AltSetting {
        iface,
        alt,
        link,
        format_type,
        formats,
        channels,
        channel_config,
        subslot,
        bits,
        endpoint,
        feedback: partial.feedback,
    });
    Ok(())
}

fn control_descriptor(function: &mut Function, at: usize, d: &[u8]) -> Result<Option<(u16, u16)>, DescError> {
    let subtype = d[2];
    let need = |n: usize| {
        if d.len() < n { Err(DescError::TooShort { at, subtype, len: d[0] }) } else { Ok(()) }
    };
    match subtype {
        ac::HEADER => {
            need(9)?;
            return Ok(Some((le16(d, 3), le16(d, 6))));
        }
        ac::CLOCK_SOURCE => {
            need(8)?;
            function.clocks.push(ClockSource { id: d[3], attributes: d[4], controls: d[5], assoc_terminal: d[6] });
        }
        ac::INPUT_TERMINAL => {
            need(17)?;
            function.inputs.push(InputTerminal {
                id: d[3],
                ty: le16(d, 4),
                assoc: d[6],
                clock: d[7],
                channels: d[8],
                channel_config: le32(d, 9),
                controls: le16(d, 14),
            });
        }
        ac::OUTPUT_TERMINAL => {
            need(12)?;
            function.outputs.push(OutputTerminal {
                id: d[3],
                ty: le16(d, 4),
                assoc: d[6],
                source: d[7],
                clock: d[8],
                controls: le16(d, 9),
            });
        }
        ac::FEATURE_UNIT => {
            // 3 + id + source + iFeature, and at least the master channel's four bytes.
            need(10)?;
            if !(d.len() - 6).is_multiple_of(4) {
                return Err(DescError::TooShort { at, subtype, len: d[0] });
            }
            let controls = d[5..d.len() - 1].as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect();
            function.features.push(FeatureUnit { id: d[3], source: d[4], controls });
        }
        _ => {
            need(4)?;
            function.other_units.push(d[3]);
        }
    }
    Ok(None)
}

fn streaming_descriptor(partial: &mut PartialAlt, at: usize, d: &[u8]) -> Result<(), DescError> {
    let subtype = d[2];
    match subtype {
        as_::GENERAL => {
            if d.len() < 16 {
                return Err(DescError::TooShort { at, subtype, len: d[0] });
            }
            partial.general = Some((d[3], d[5], le32(d, 6), d[10], le32(d, 11)));
        }
        as_::FORMAT_TYPE => {
            if d.len() < 6 {
                return Err(DescError::TooShort { at, subtype, len: d[0] });
            }
            partial.format = Some((d[4], d[5]));
        }
        _ => {}
    }
    Ok(())
}

impl Function {
    /// The whole configuration descriptor. Only the UAC2 function in it is kept; other interfaces are stepped over.
    pub fn parse(config: &[u8]) -> Result<Function, DescError> {
        if config.len() < 9 {
            return Err(DescError::Truncated { at: 0 });
        }
        if config[1] != DT_CONFIGURATION {
            return Err(DescError::NotAConfiguration);
        }
        let total = le16(config, 2);
        if total as usize > config.len() {
            return Err(DescError::ConfigLength { total, have: config.len() });
        }
        let bytes = &config[..total as usize];
        let mut function = Function::default();
        let mut have_control = false;
        let mut current = Current::Other;
        let mut partial = PartialAlt::default();
        let (mut declared, mut walked) = (None, 0usize);
        let mut at = 0;
        while at < bytes.len() {
            let len = bytes[at] as usize;
            if len < 2 || at + len > bytes.len() {
                return Err(DescError::Truncated { at });
            }
            let d = &bytes[at..at + len];
            match d[1] {
                DT_INTERFACE => {
                    if len < 9 {
                        return Err(DescError::TooShort { at, subtype: 0, len: d[0] });
                    }
                    finish(&mut function, current, core::mem::take(&mut partial))?;
                    current = match (d[5], d[6], d[7]) {
                        (CLASS_AUDIO, SUBCLASS_CONTROL, PROTOCOL_UAC2) => {
                            if have_control {
                                return Err(DescError::SecondControlInterface(d[2]));
                            }
                            have_control = true;
                            function.control_iface = d[2];
                            Current::Control
                        }
                        (CLASS_AUDIO, SUBCLASS_STREAMING, PROTOCOL_UAC2) => {
                            Current::Streaming { iface: d[2], alt: d[3] }
                        }
                        _ => Current::Other,
                    };
                }
                DT_ENDPOINT => {
                    if len < 7 {
                        return Err(DescError::TooShort { at, subtype: 0, len: d[0] });
                    }
                    if let Current::Streaming { .. } = current {
                        let ep = Endpoint { address: d[2], attributes: d[3], max_packet: le16(d, 4), interval: d[6] };
                        // An asynchronous OUT stream's feedback endpoint may follow its data endpoint; taken for the
                        // data one it would carry the sound to the wrong place.
                        if ep.is_feedback() {
                            partial.feedback = Some(ep.address);
                        } else if partial.endpoint.is_none() {
                            partial.endpoint = Some(ep);
                        }
                    }
                }
                DT_CS_INTERFACE => {
                    if len < 3 {
                        return Err(DescError::Truncated { at });
                    }
                    match current {
                        Current::Control => {
                            walked += len;
                            if let Some((adc, length)) = control_descriptor(&mut function, at, d)? {
                                if adc != ADC_2_00 {
                                    return Err(DescError::AdcVersion(adc));
                                }
                                declared = Some(length);
                            }
                        }
                        Current::Streaming { .. } => streaming_descriptor(&mut partial, at, d)?,
                        Current::Other => {}
                    }
                }
                _ => {}
            }
            at += len;
        }
        finish(&mut function, current, partial)?;
        if !have_control {
            return Err(DescError::NoControlInterface);
        }
        let length = declared.ok_or(DescError::NoControlHeader)?;
        if length as usize > walked {
            return Err(DescError::ControlLength { total: length, walked });
        }
        Ok(function)
    }
}
