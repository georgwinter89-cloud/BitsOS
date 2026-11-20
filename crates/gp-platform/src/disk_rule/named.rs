//! GP-DISK (Georg 15.11.: "wie am Mini-PC eine eigene Partition für BitsOS"): the one NVMe controller Georg names in
//! the stick's CONFIG.TXT may get the NVMe driver under the Gaming-PC profile; every other mass-storage function stays
//! untouched as `nvme_bind=off` keeps it. The line names the place **and** the ids Georg reads off the report page
//! (`gp_nvme=81:00.0/1d97:5236`): a drive moved to another slot, or another drive in that slot, does not match and
//! stays untouched. Georg's Windows and CachyOS drives can never be named ([`NEVER_NAMED`]), whatever the line says.
//!
//! The ids are compared with what the PCI scan reads from the function's 64-byte header (the read an untouched
//! function allows), never with something the drive says about itself.

use alloc::format;
use alloc::string::String;

/// The CONFIG key (guarded: only Georg's own edit adds or changes it).
pub const NVME_KEY: &str = "gp_nvme";

/// PCI class of an NVMe controller (`PCI_CLASS_STORAGE_EXPRESS` 0x010802, linux-7.3.1/include/linux/pci_ids.h:27).
pub const CLASS_NVME: u32 = 0x01_08_02;

/// Georg's Samsung 990 EVO Plus (Windows, 01:00.0) and 980 PRO (CachyOS, 86:00.0): never named, never a driver.
pub const NEVER_NAMED: &[(u16, u16)] = &[(0x144D, 0xA80D), (0x144D, 0xA80A)];

/// What `gp_nvme=` names: a place in PCI segment 0 and the ids expected there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Named {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
}

fn hex<T: TryFrom<u32>>(t: &str, digits: usize) -> Option<T> {
    if t.len() != digits || !t.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    T::try_from(u32::from_str_radix(t, 16).ok()?).ok()
}

impl Named {
    /// `bb:dd.f/vvvv:dddd`, hex in any case, optionally `0000:` before the bus (lspci -D writes it). Anything else
    /// is refused: a line that does not say exactly one controller releases nothing.
    pub fn parse(v: &str) -> Result<Named, &'static str> {
        let v = v.trim();
        let (place, ids) = v.split_once('/').ok_or("no / between the place and the ids")?;
        let place = place.strip_prefix("0000:").unwrap_or(place);
        let (bus, rest) = place.split_once(':').ok_or("the place is not bb:dd.f")?;
        let (dev, func) = rest.split_once('.').ok_or("the place is not bb:dd.f")?;
        let (vendor, device) = ids.split_once(':').ok_or("the ids are not vvvv:dddd")?;
        let bus = hex::<u8>(bus, 2).ok_or("the bus is not two hex digits")?;
        let dev = hex::<u8>(dev, 2).filter(|d| *d < 32).ok_or("the device is not 00 to 1f")?;
        let func = hex::<u8>(func, 1).filter(|f| *f < 8).ok_or("the function is not 0 to 7")?;
        let vendor = hex::<u16>(vendor, 4).ok_or("the vendor id is not four hex digits")?;
        let device = hex::<u16>(device, 4).ok_or("the device id is not four hex digits")?;
        if vendor == 0xFFFF || vendor == 0 {
            return Err("no vendor has that id");
        }
        Ok(Named { bus, dev, func, vendor, device })
    }

    /// The value as the report page shows it for Georg to copy.
    pub fn render(&self) -> String {
        format!("{:02x}:{:02x}.{}/{:04x}:{:04x}", self.bus, self.dev, self.func, self.vendor, self.device)
    }

    pub fn place(&self) -> (u8, u8, u8) {
        (self.bus, self.dev, self.func)
    }
}

/// Why a `gp_nvme=` line releases nothing (the report page and the PCI log say it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Unreadable(&'static str),
    /// Georg's Windows or CachyOS drive.
    NeverNamed,
}

/// The line as the rule reads it: a controller, nothing (no line), or a refusal.
pub fn read(v: Option<&str>) -> Option<Result<Named, Refusal>> {
    let v = v?;
    if v.trim().is_empty() {
        return None;
    }
    Some(match Named::parse(v) {
        Err(e) => Err(Refusal::Unreadable(e)),
        Ok(n) if NEVER_NAMED.contains(&(n.vendor, n.device)) => Err(Refusal::NeverNamed),
        Ok(n) => Ok(n),
    })
}

/// The controller a `gp_nvme=` line releases, if any.
pub fn named(v: Option<&str>) -> Option<Named> {
    read(v).and_then(Result::ok)
}

/// What the rule says about one function at the PCI scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Not a storage function, or no `nvme_bind=off`: the rule has nothing to say (as before).
    Free,
    /// The one controller `gp_nvme=` names, at its place with its ids: it may get the NVMe driver.
    Released,
    /// Untouched (`nvme_bind=off`).
    Kept,
    /// Untouched: the place `gp_nvme=` names holds another function (other ids, or not an NVMe controller).
    Mismatch,
}

/// The rule for the function at `place` with `ids` and `class`, under CONFIG's `nvme_bind` and `gp_nvme` values.
pub fn verdict(class: u32, nvme_bind: Option<&str>, gp_nvme: Option<&str>, place: (u8, u8, u8), ids: (u16, u16)) -> Verdict {
    if !super::kept_unbound(class, nvme_bind) {
        return Verdict::Free;
    }
    match named(gp_nvme) {
        Some(n) if n.place() == place => {
            if (n.vendor, n.device) == ids && class == CLASS_NVME && !NEVER_NAMED.contains(&ids) {
                Verdict::Released
            } else {
                Verdict::Mismatch
            }
        }
        _ => Verdict::Kept,
    }
}

/// `true`: the function stays untouched (no BAR sizing, no command write, no driver).
pub fn kept_unbound_at(class: u32, nvme_bind: Option<&str>, gp_nvme: Option<&str>, place: (u8, u8, u8), ids: (u16, u16)) -> bool {
    matches!(verdict(class, nvme_bind, gp_nvme, place, ids), Verdict::Kept | Verdict::Mismatch)
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;

    const LEXAR: (u16, u16) = (0x1D97, 0x5236);
    const LINE: Option<&str> = Some("81:00.0/1d97:5236");

    #[test]
    fn the_line_names_one_controller_exactly() {
        let n = Named::parse("81:00.0/1d97:5236").unwrap();
        assert_eq!(n, Named { bus: 0x81, dev: 0, func: 0, vendor: 0x1D97, device: 0x5236 });
        assert_eq!(n.render(), "81:00.0/1d97:5236");
        for same in [" 81:00.0/1d97:5236\r", "0000:81:00.0/1D97:5236", "81:00.0/1D97:5236"] {
            assert_eq!(Named::parse(same), Ok(n), "{same:?}");
        }
        for bad in ["", "81:00.0", "1d97:5236", "81:00/1d97:5236", "81:0.0/1d97:5236", "81:20.0/1d97:5236", "81:00.8/1d97:5236", "181:00.0/1d97:5236",
            "81:00.0/1d97:523", "81:00.0/ffff:5236", "81:00.0/0000:5236", "81:00.0/1d97-5236", "81:00.0 / 1d97:5236", "0001:81:00.0/1d97:5236", "+1:00.0/1d97:5236"] {
            assert!(Named::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn georgs_windows_and_cachyos_drives_are_never_named() {
        for (v, d) in NEVER_NAMED {
            for place in ["01:00.0", "86:00.0", "81:00.0"] {
                let line = format!("{place}/{v:04x}:{d:04x}");
                assert_eq!(read(Some(&line)), Some(Err(Refusal::NeverNamed)), "{line}");
                assert_eq!(named(Some(&line)), None);
                let (b, dv) = (u8::from_str_radix(&place[..2], 16).unwrap(), 0);
                assert_eq!(verdict(CLASS_NVME, Some("off"), Some(&line), (b, dv, 0), (*v, *d)), Verdict::Kept, "{line}");
            }
        }
        assert_eq!(read(None), None);
        assert_eq!(read(Some("  ")), None, "an emptied line (the loader's) is no line");
        assert!(matches!(read(Some("x")), Some(Err(Refusal::Unreadable(_)))));
    }

    /// Georg's three drives with the line for the Lexar: only the Lexar is released; with the line wrong in any
    /// field, or without `nvme_bind=off`, the answer is what it was before this key existed.
    #[test]
    fn only_the_named_place_with_its_ids_is_released() {
        let samsung_w = (0x144D, 0xA80D);
        let samsung_c = (0x144D, 0xA80A);
        let drives = [((0x01, 0, 0), samsung_w), ((0x81, 0, 0), LEXAR), ((0x86, 0, 0), samsung_c)];
        for (place, ids) in drives {
            let v = verdict(CLASS_NVME, Some("off"), LINE, place, ids);
            assert_eq!(v, if ids == LEXAR { Verdict::Released } else { Verdict::Kept }, "{place:?}");
            assert_eq!(kept_unbound_at(CLASS_NVME, Some("off"), LINE, place, ids), ids != LEXAR);
            assert_eq!(kept_unbound_at(CLASS_NVME, Some("off"), None, place, ids), true, "no line: all untouched");
            assert_eq!(verdict(CLASS_NVME, None, LINE, place, ids), Verdict::Free, "the Mini-PC: no nvme_bind line");
            assert_eq!(verdict(CLASS_NVME, Some("on"), LINE, place, ids), Verdict::Free);
        }
        // The Samsung moved into the Lexar's slot, or the Lexar moved away: nothing released.
        assert_eq!(verdict(CLASS_NVME, Some("off"), LINE, (0x81, 0, 0), samsung_c), Verdict::Mismatch, "another drive in the named slot");
        assert_eq!(verdict(CLASS_NVME, Some("off"), LINE, (0x81, 0, 0), (0x1D97, 0x5237)), Verdict::Mismatch);
        assert_eq!(verdict(CLASS_NVME, Some("off"), LINE, (0x82, 0, 0), LEXAR), Verdict::Kept);
        // The place right, but no NVMe controller there (AHCI, VMD): no driver of BitsOS binds those anyway.
        for class in [0x01_06_01, 0x01_04_00, 0x01_08_03, 0x01_08_01] {
            assert_eq!(verdict(class, Some("off"), LINE, (0x81, 0, 0), LEXAR), Verdict::Mismatch, "{class:#x}");
        }
        // Not storage: the key says nothing about it.
        assert_eq!(verdict(0x02_00_00, Some("off"), Some("83:00.0/10ec:8126"), (0x83, 0, 0), (0x10EC, 0x8126)), Verdict::Free);
        // A garbled line keeps everything untouched.
        for bad in ["81:00.0", "81:00.0/1d97", "on", "81:00.0/1d97:5236/x"] {
            assert_eq!(verdict(CLASS_NVME, Some("off"), Some(bad), (0x81, 0, 0), LEXAR), Verdict::Kept, "{bad:?}");
        }
    }
}
