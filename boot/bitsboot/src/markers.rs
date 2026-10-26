//! The only place bitsboot writes: the generation marker `TRY` (V2-PLAN §7 rule 3), through the boot
//! volume's own SimpleFileSystem, and only at a path `bitsboot_pure::gens::is_marker_path` accepts
//! (`\BITSOS2\SYS\<n>\TRY`, R8b). Never UEFI variables, never another volume, never a delete or rename.

use crate::{Ctx, R, fail, files};
use bitsboot_pure::gens;
use log::info;
use uefi::CString16;
use uefi::proto::media::file::{Directory, File, FileAttribute, FileMode};

/// Writes generation `number`'s `TRY` (an empty file), flushes it and reads it back. `NEW` stays: `TRY` beats
/// `NEW` in the rule, and creating one empty file is a single directory entry, whereas a FAT rename (EDK2
/// FatSetInfo) removes the old entry and writes a new one, so a reset in between could leave no marker at all
/// and the generation would lose its one-try limit.
pub fn mark_try(root: &mut Directory, number: u32) -> R<()> {
    let tried = gens::path(number, "TRY");
    if !gens::is_marker_path(&tried) {
        return Err(fail!("refusing to write {tried}: not a generation marker below {}", gens::SYS));
    }
    let name = CString16::try_from(tried.as_str()).map_err(|_| fail!("bad path {tried}"))?;
    let mut h = root.open(&name, FileMode::CreateReadWrite, FileAttribute::empty()).ctx("cannot create TRY")?;
    h.flush().ctx("Flush after creating TRY failed")?;
    h.close();
    if !files::is_file(root, &tried)? {
        return Err(fail!("{tried} is not there after writing it"));
    }
    info!("bitsboot marker.written: {tried}");
    Ok(())
}
