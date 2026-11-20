//! UPD-4: which parts the generation that runs names, read from the boot volume before it is mounted
//! (`fs_fat::binds`): `\BITSOS2\SYS\<n>\FILES` of the GENERATION record bitsboot (or a Reload) handed over, each
//! part only when its record holds the hash FILES names and its folder is there. A generation without FILES (from
//! before UPD-4, or installed by `install_kernel`) binds nothing and runs with the medium's own folders.

use super::parts::SMALL_MAX;
use super::parts_rules as rules;
use alloc::format;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use fs_fat::binds::Bind;
use fs_fat::vol::Volume;

fn read(vol: &Volume, path: &[&[u8]]) -> Option<Vec<u8>> {
    let e = vol.peek(path).ok().flatten().filter(|e| !e.is_dir())?;
    vol.peek_read(&e, SMALL_MAX).ok()
}

/// The binds for generation `n` on `vol` (pure on the volume's content).
pub fn binds_for(vol: &Volume, n: u32) -> Vec<Bind> {
    let slot = format!("{n}");
    let Some(t) = read(vol, &[b"BITSOS2", b"SYS", slot.as_bytes(), rules::FILES.as_bytes()]) else { return Vec::new() };
    let parts = match rules::files_parse(&t) {
        Ok(p) => p,
        Err(e) => {
            klog::event!(Warn, "kexec", "parts.files_bad", generation = n, error = e.as_str(); "nothing bound: the medium's own folders show");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for (name, hash) in parts {
        let rec = rules::record_name(&name, &hash);
        let folder = rules::folder(&name, &hash);
        let sealed = read(vol, &[b"BITSOS2", b"SYS", rules::STORE.as_bytes(), rec.as_bytes()])
            .and_then(|t| rules::record_parse(&t).ok())
            .is_some_and(|(rn, rh, _, _)| rn == name && rh == hash);
        let dir = vol.peek(&[b"BITSOS2", b"SYS", rules::STORE.as_bytes(), folder.as_bytes()]).ok().flatten().is_some_and(|e| e.is_dir());
        if !sealed || !dir {
            klog::event!(Warn, "kexec", "parts.missing", generation = n, part = name.as_str(), hash = hash.as_str(); "not bound: the medium's own folder shows");
            continue;
        }
        out.push(Bind {
            at: vec![b"BITSOS2".to_vec(), name.as_bytes().to_vec()],
            from: vec![b"BITSOS2".to_vec(), b"SYS".to_vec(), rules::STORE.as_bytes().to_vec(), folder.into_bytes()],
            what: format!("{name} {}", &hash[..16]),
        });
    }
    out
}

fn plan(vol: &Arc<Volume>) -> Vec<Bind> {
    match crate::boot::boot_info().and_then(|(bi, _)| bi.generation()) {
        Some(g) if g.number > 0 => binds_for(vol, g.number),
        _ => Vec::new(),
    }
}

fn init() -> Result<(), &'static str> {
    fs_fat::binds::set_planner(plan);
    Ok(())
}

// Subsys: before the stick's listener (Service) can mount anything.
kreg::initcall!(kreg::Level::Subsys, "kexec.parts_boot", init);
