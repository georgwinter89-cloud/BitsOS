//! System generations `\BITSOS2\SYS\<n>\` on the boot volume: what each directory holds (files and markers)
//! and the PIN. The choice is `bitsboot_pure::gens::choose`; the one marker write is in `markers.rs`.

use crate::files;
use crate::{R, fail};
use alloc::vec::Vec;
use bitsboot_pure::gens::{GenDir, Pin, SYS, parse_name, parse_pin};
use log::{info, warn};
use uefi::proto::media::file::Directory;

pub use bitsboot_pure::gens::path;

/// Every numbered directory below `\BITSOS2\SYS` with its files and markers, logged one line each. A
/// directory that cannot be listed counts as empty (no KERNEL: its generation is "bad files").
pub fn scan(root: &mut Directory) -> R<Vec<GenDir>> {
    let Some(entries) = files::list(root, SYS)? else {
        return Err(fail!("{SYS} is missing on the boot stick (no system generation installed)"));
    };
    let mut gens = Vec::new();
    for e in entries.iter().filter(|e| e.is_dir) {
        let Some(number) = parse_name(&e.name) else { continue };
        let mut g = GenDir { number, ..Default::default() };
        let dir = alloc::format!("{SYS}\\{number}");
        match files::list(root, &dir) {
            Ok(Some(list)) => list.iter().for_each(|f| g.note(&f.name, f.is_dir)),
            Ok(None) => warn!("{dir} vanished while listing"),
            Err(f) => warn!("{dir}: {}", f.0),
        }
        gens.push(g);
    }
    gens.sort_unstable_by_key(|g| g.number);
    for g in &gens {
        let has = |b: bool, n: &'static str| if b { n } else { "" };
        info!(
            "generation {}: {} {} ({})",
            g.number,
            has(g.kernel, "KERNEL"),
            has(g.initrd, "INITRD"),
            g.markers()
        );
    }
    Ok(gens)
}

/// `\BITSOS2\SYS\PIN` (V2-PLAN §7 rule 1, B1 "system back to ...").
pub fn read_pin(root: &mut Directory) -> Pin {
    let path = alloc::format!("{SYS}\\PIN");
    match files::read(root, &path) {
        Ok(None) if files::is_dir(root, &path).unwrap_or(false) => {
            warn!("bitsboot pin.invalid: {path} is a directory, not a file; ignored");
            Pin::Invalid
        }
        Ok(None) => Pin::None,
        Ok(Some(text)) => match parse_pin(&text) {
            Some(n) => {
                info!("{path}: generation {n}");
                Pin::Number(n)
            }
            None => {
                warn!("bitsboot pin.invalid: {path} holds no generation number ({} bytes); ignored", text.len());
                Pin::Invalid
            }
        },
        Err(f) => {
            warn!("bitsboot pin.unreadable: {path}: {}; ignored", f.0);
            Pin::Invalid
        }
    }
}
