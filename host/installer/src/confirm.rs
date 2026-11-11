//! The human's yes before a disk is written. Erasing needs the word ERASE and the disk's id exactly as listed
//! (`ERASE sdb-31.0G-3fa2`); adding a release to a BitsOS stick needs UPDATE and the id. An agent passes the same id
//! with `--erase <id>` / `--update <id>`, and the id has to come from the person who looked at the list. The id
//! carries a fingerprint of the stick's vendor, model and serial, so an id from an earlier `--list` names that
//! stick only; a stick without a serial cannot be told from its twin that way and is chosen interactively.

use crate::devices::Disk;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Erase,
    Update,
}

impl Action {
    pub fn word(self) -> &'static str {
        match self {
            Action::Erase => "ERASE",
            Action::Update => "UPDATE",
        }
    }
}

/// The line the human types interactively: `<WORD> <id>`, nothing else (the line end aside).
pub fn typed_ok(action: Action, d: &Disk, line: &str) -> bool {
    let line = line.strip_suffix('\n').unwrap_or(line);
    let line = line.strip_suffix('\r').unwrap_or(line);
    line == format!("{} {}", action.word(), d.id)
}

/// The id given with `--erase`/`--update`: exactly one offered disk's id, character for character.
pub fn pick<'a>(disks: &'a [Disk], given: &str) -> Result<&'a Disk, String> {
    match disks.iter().find(|d| d.id == given) {
        Some(d) if d.offered() && d.serial.is_empty() => Err(format!(
            "refused: {} reports no serial number, so an id from an earlier list cannot prove it is still the same stick. Nothing was written. Run the installer without --erase/--update and choose it from its own list.",
            d.id
        )),
        Some(d) if d.offered() => Ok(d),
        Some(d) => Err(format!("refused: {} is not offered: {}", d.id, d.why_not().unwrap_or_default())),
        None => {
            let ids: Vec<&str> = disks.iter().filter(|d| d.offered()).map(|d| d.id.as_str()).collect();
            Err(format!("refused: no disk has the id '{given}' (offered: {}). Show the list (--list) to the person and use the id they confirm.", if ids.is_empty() { "none".to_string() } else { ids.join(", ") }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn disk(id: &str, system: Option<&str>) -> Disk {
        Disk {
            id: id.into(),
            path: PathBuf::from("/dev/x"),
            vendor: "V".into(),
            model: "M".into(),
            serial: "S".into(),
            size: 31_004_295_168,
            removable: true,
            usb: true,
            mounted: vec![],
            system: system.map(String::from),
        }
    }

    #[test]
    fn the_typed_line_must_be_exact() {
        let d = disk("sdb-31.0G", None);
        assert!(typed_ok(Action::Erase, &d, "ERASE sdb-31.0G\n"));
        assert!(typed_ok(Action::Erase, &d, "ERASE sdb-31.0G\r\n"));
        assert!(typed_ok(Action::Update, &d, "UPDATE sdb-31.0G"));
        for bad in ["erase sdb-31.0G", "ERASE sdb", "ERASE  sdb-31.0G", " ERASE sdb-31.0G", "ERASE sdb-31.0G ", "ERASE sdc-31.0G", "J", "y", "yes", "", "UPDATE sdb-31.0G"] {
            assert!(!typed_ok(Action::Erase, &d, bad), "{bad:?}");
        }
    }

    #[test]
    fn an_id_from_an_earlier_list_does_not_fit_another_stick() {
        let listed = crate::devices::disk_id("sdb", 31_004_295_168, "V", "M", "S");
        let mut swapped = disk(&crate::devices::disk_id("sdb", 31_004_295_168, "V", "M", "T"), None);
        swapped.serial = "T".into();
        assert!(pick(std::slice::from_ref(&swapped), &listed).unwrap_err().contains("no disk"));
        let mut bare = disk(&crate::devices::disk_id("sdb", 31_004_295_168, "V", "M", ""), None);
        bare.serial.clear();
        assert!(pick(std::slice::from_ref(&bare), &bare.id).unwrap_err().contains("no serial"));
    }

    #[test]
    fn a_given_id_names_one_offered_disk() {
        let disks = [disk("sdb-31.0G", None), disk("sdc-1000.2G", Some("it holds the running system's /"))];
        assert_eq!(pick(&disks, "sdb-31.0G").unwrap().id, "sdb-31.0G");
        assert!(pick(&disks, "sdc-1000.2G").unwrap_err().contains("not offered"));
        assert!(pick(&disks, "sdb").unwrap_err().contains("no disk"));
        assert!(pick(&disks, "SDB-31.0G").is_err());
        assert!(pick(&disks, "/dev/sdb").is_err());
    }
}
