//! The protocol's file on the boot partition (the Lexar's BitsOS partition on the Gaming-PC, the volume
//! `fs_fat::stick` mounted at /boot): one file per run below `\BITSOS2\REPORT\`, the head written, flushed and
//! read back before anything of the card is touched, then each part appended to the one open file and made
//! durable at once (`Inode::sync(Fsync)`: FAT's fsync writes the dirty pages and the directory entry, then the
//! volume's sync, which writes FSInfo's count and SYNCHRONIZE CACHE), so a hang or a hard power-off keeps every
//! line up to it. Only with `gp_stick=on`, as the report's file.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use amdgpu::rdna4::messung::Disk;
use vfs::pagecache::SyncKind;

/// The folder, as the report's (`gp_report::file::DIR`).
pub const DIR: &str = gp_report::file::DIR;

/// The boot partition, if the run may write there: Georg's `gp_stick=on`, mounted, writable.
pub fn volume() -> Result<Arc<fs_fat::Volume>, String> {
    if crate::config::get(gp_platform::tiers::STICK_KEY).as_deref().map(str::trim) != Some("on") {
        return Err(String::from("gp_stick ist nicht an: das Protokoll darf nicht auf den Startdatenträger"));
    }
    let vol = fs_fat::stick::wait_mounted(30_000).ok_or("der Startdatenträger ist nicht eingebunden")?;
    if !vol.writable() {
        return Err(format!("der Startdatenträger wird nur gelesen ({})", fs_fat::stick::last_reason().unwrap_or("kein Grund bekannt")));
    }
    Ok(vol)
}

#[derive(Default)]
pub struct FileDisk {
    /// The volume given by a test; the run's own is `volume()`.
    vol: Option<Arc<fs_fat::Volume>>,
    open: Option<Open>,
}

struct Open {
    d: Arc<vfs::Dentry>,
    at: u64,
}

impl FileDisk {
    /// A disk on a volume a test gives (its own checks of the volume done).
    pub fn on(vol: Arc<fs_fat::Volume>) -> FileDisk {
        FileDisk { vol: Some(vol), open: None }
    }
}

impl Disk for FileDisk {
    fn open(&mut self, head: &str) -> Result<String, String> {
        if self.vol.is_none() {
            volume()?;
        }
        let ns = vfs::root_ns();
        let names: Vec<String> = ns.list_dir(DIR).map(|l| l.into_iter().map(|(n, _)| String::from_utf8_lossy(&n).into_owned()).collect()).unwrap_or_default();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let name = gp_report::file::messung_next(&refs).ok_or("im Ordner sind alle 9999 Namen der Messung vergeben")?;
        let path = format!("{DIR}/{name}");
        let d = ns.create_file_bytes(path.as_bytes(), 0o644, head.as_bytes()).map_err(|e| format!("{path} ließ sich nicht anlegen (Fehler {e})"))?;
        d.inode.sync(SyncKind::Fsync).map_err(|e| format!("{path} ließ sich nicht auf die Platte schreiben (Fehler {e})"))?;
        let back = ns.read_file(&path, head.len() + 1).map_err(|e| format!("{path} ließ sich nicht zurücklesen (Fehler {e})"))?;
        if back != head.as_bytes() {
            return Err(format!("{path} liest sich anders zurück, als es geschrieben wurde ({} statt {} Bytes)", back.len(), head.len()));
        }
        klog::event!(Info, "gx", "rdna4.messung.file", path = path.as_str(), bytes = head.len());
        self.open = Some(Open { d, at: head.len() as u64 });
        Ok(name)
    }

    fn put(&mut self, text: &str) -> Result<(), String> {
        let o = self.open.as_mut().ok_or("das Protokoll ist nicht offen")?;
        let n = {
            let _w = o.d.sb.write_begin().map_err(|e| format!("Fehler {e}"))?;
            o.d.inode.write_at(o.at, text.as_bytes()).map_err(|e| format!("Fehler {e} beim Schreiben"))?
        };
        if n != text.len() {
            return Err(format!("nur {n} von {} Bytes geschrieben", text.len()));
        }
        o.at += n as u64;
        o.d.inode.sync(SyncKind::Fsync).map_err(|e| format!("Fehler {e} beim Sichern auf der Platte"))
    }
}
