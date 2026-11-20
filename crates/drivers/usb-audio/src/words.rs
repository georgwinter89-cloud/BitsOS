//! The plain German the tone check and the report page show (GP-SOUND): what an output is, and what a device or the
//! controller answered, without code words. The log keeps the exact values in English.

use crate::uac2dev::{JackError, StartError};
use alloc::format;
use alloc::string::String;
use uac2::desc::{DescError, terminal};
use uac2::plan::PlanError;
use uac2::range::RangeError;
use usb_core::UsbError;

pub fn jack_name(ty: u16) -> &'static str {
    match ty {
        terminal::SPEAKER => "Lautsprecher-Buchse",
        terminal::HEADPHONES => "Kopfhörer-Buchse",
        terminal::SPDIF => "S/PDIF-Ausgang",
        _ => "Buchse",
    }
}

pub fn usb(e: UsbError) -> String {
    match e {
        UsbError::Gone => String::from("Gerät weg"),
        UsbError::Timeout => String::from("keine Antwort in der Zeit"),
        UsbError::Stall => String::from("Gerät lehnt ab"),
        UsbError::Busy => String::from("belegt"),
        UsbError::Command(8) => String::from("zu wenig Bandbreite auf dem Bus"),
        UsbError::Command(35) => String::from("zu wenig Bandbreite im Übersetzer des Hubs"),
        UsbError::Command(c) => format!("Controller lehnt ab (Code {c})"),
        UsbError::Transfer(c) => format!("Übertragung fehlgeschlagen (Code {c})"),
        other => String::from(other.as_str()),
    }
}

pub fn hz(r: Option<u32>) -> String {
    r.map_or_else(|| String::from("nichts"), |r| format!("{r} Hz"))
}

pub fn start(e: &StartError, clock: u8, iface: u8, alt: u8, rate: u32) -> String {
    match e {
        StartError::Deselect(x) => format!("Schnittstelle {iface} ließ sich nicht abschalten: {}", usb(*x)),
        StartError::SetRate { err, before } => format!("Takt {clock} war {}, {rate} Hz abgelehnt: {}", hz(*before), usb(*err)),
        StartError::Invalid { rate } => format!("Takt {clock} meldet ungültig (bei {})", hz(*rate)),
        StartError::Select(x) => format!("Einstellung {alt} der Schnittstelle {iface} abgelehnt: {}", usb(*x)),
    }
}

fn plan(e: &PlanError) -> String {
    match e {
        PlanError::NoSuchJack(_) => String::from("das Gerät hat sie nicht"),
        PlanError::PathUnsupported { unit } => format!("der Weg führt über Baustein {unit} (Mischer oder Wahlschalter), den BitsOS nicht kennt"),
        PlanError::NoUsbSide(_) => String::from("keine Verbindung zur USB-Seite"),
        PlanError::NoFormat { .. } => String::from("kein 16-Bit-Stereo"),
        PlanError::ClockNotASource(id) => format!("Takt {id} ist ein Wahlschalter oder Vervielfacher, das ist nicht gebaut"),
        PlanError::ClockFixed(id) => format!("Takt {id} lässt sich nicht einstellen"),
        PlanError::RateNotOffered(r) => format!("der Takt bietet {r} Hz nicht an"),
        PlanError::BadEndpoint(a) => format!("Endpunkt {a:#04x} passt nicht"),
        PlanError::PacketTooSmall { need, have } => format!("Pakete zu klein ({have} statt {need} Bytes)"),
        PlanError::NeedsFeedback(_) => String::from("bräuchte einen Rückmelde-Endpunkt, das ist nicht gebaut"),
    }
}

pub fn jack(e: &JackError) -> String {
    match e {
        JackError::Plan(p) => plan(p),
        JackError::Range(x) => format!("Takt-Bereich nicht gelesen: {}", usb(*x)),
        JackError::RangeBytes(RangeError::Truncated { .. }) => String::from("Takt-Bereich zu kurz"),
        JackError::RangeBytes(RangeError::TooManyTriplets(n)) => format!("Takt-Bereich mit {n} Einträgen, zu viele"),
        JackError::NoEndpoint(a) => format!("Endpunkt {a:#04x} fehlt in der Beschreibung"),
        JackError::DescriptorChanged(a) => format!("Gerät beschreibt Endpunkt {a:#04x} beim zweiten Lesen anders, nicht benutzt"),
        JackError::Start { error, clock, iface, alt } => start(error, *clock, *iface, *alt, crate::uac2dev::RATE),
    }
}

pub fn descriptors(e: &DescError) -> String {
    match e {
        DescError::SecondControlInterface(_) => String::from("zwei Ton-Funktionen in einem Gerät, nur eine ist gebaut"),
        DescError::AdcVersion(v) => format!("Ton-Beschreibung Version {v:#06x} statt 2.00"),
        _ => String::from("Ton-Beschreibung kaputt, ganz abgelehnt"),
    }
}
