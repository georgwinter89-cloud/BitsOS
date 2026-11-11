//! The stick's FAT32 through the `fatfs` crate: formatting the partition, and the few operations the installer
//! needs (folders by path, a file from a stream, a whole folder removed, a rename, small files read).

use crate::blockio::Window;
use fatfs::{Date, DateTime, Dir, FatType, FileSystem, FormatVolumeOptions, FsOptions, ReadWriteSeek, Time, TimeProvider};
use std::io::{self, Read, Seek, SeekFrom, Write};

/// The file system on a window that stays the caller's, so it can be synced after `unmount`.
pub type Fs<'w, 'a> = FileSystem<&'w mut Window<'a>>;

pub const LABEL: [u8; 11] = *b"BITSOS     ";

/// File times in UTC (the stick has no time zone; BitsOS reads them as UTC).
#[derive(Debug)]
struct Utc;

fn now() -> DateTime {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = (yoe + era * 400 + if month <= 2 { 1 } else { 0 }).clamp(1980, 2107);
    DateTime {
        date: Date { year: year as u16, month: month as u16, day: day as u16 },
        time: Time { hour: (rem / 3600) as u16, min: (rem % 3600 / 60) as u16, sec: (rem % 60) as u16, millis: 0 },
    }
}

impl TimeProvider for Utc {
    fn get_current_date(&self) -> Date {
        now().date
    }
    fn get_current_date_time(&self) -> DateTime {
        now()
    }
}

static UTC: Utc = Utc;

pub fn format(w: &mut Window, volume_id: u32) -> io::Result<()> {
    w.seek(SeekFrom::Start(0))?;
    // Two FATs (BitsOS writes only volumes whose FATs are mirrored), label BITSOS.
    fatfs::format_volume(&mut *w, FormatVolumeOptions::new().fat_type(FatType::Fat32).fats(2).volume_label(LABEL).volume_id(volume_id))?;
    w.flush()
}

pub fn mount<'w, 'a>(w: &'w mut Window<'a>) -> io::Result<Fs<'w, 'a>> {
    w.seek(SeekFrom::Start(0))?;
    FileSystem::new(w, FsOptions::new().update_accessed_date(false).time_provider(&UTC))
}

/// The volume's label as BitsOS reads it: the root's label entry, else the boot sector's.
pub fn label<T: ReadWriteSeek>(fs: &FileSystem<T>) -> String {
    match fs.read_volume_label_from_root_dir() {
        Ok(Some(l)) => l.trim_end().to_string(),
        _ => fs.volume_label().trim_end().to_string(),
    }
}

/// Path components compare case-insensitively on FAT; this finds an entry's stored name.
pub fn find<'b, T: ReadWriteSeek>(dir: &Dir<'b, T>, name: &str) -> Option<fatfs::DirEntry<'b, T>> {
    dir.iter().flatten().find(|e| e.file_name().eq_ignore_ascii_case(name))
}

pub fn exists<T: ReadWriteSeek>(root: &Dir<T>, path: &str) -> bool {
    let mut d = root.clone();
    let parts: Vec<&str> = path.split('/').collect();
    for (i, p) in parts.iter().enumerate() {
        match find(&d, p) {
            Some(_) if i + 1 == parts.len() => return true,
            Some(e) if e.is_dir() => d = e.to_dir(),
            _ => return false,
        }
    }
    false
}

pub fn is_dir<T: ReadWriteSeek>(root: &Dir<T>, path: &str) -> bool {
    root.open_dir(path).is_ok()
}

pub fn mkdir_p<T: ReadWriteSeek>(root: &Dir<T>, path: &str) -> io::Result<()> {
    let mut d = root.clone();
    for p in path.split('/').filter(|p| !p.is_empty()) {
        d = d.create_dir(p)?;
    }
    Ok(())
}

/// Writes `size` bytes from `data` to `path` (parents made, an old file replaced).
pub fn write_file<T: ReadWriteSeek>(root: &Dir<T>, path: &str, size: u64, data: &mut dyn Read) -> io::Result<()> {
    if let Some((parent, _)) = path.rsplit_once('/') {
        mkdir_p(root, parent)?;
    }
    let mut f = root.create_file(path)?;
    f.truncate()?;
    let mut buf = vec![0u8; 1 << 20];
    let mut left = size;
    while left > 0 {
        let n = data.read(&mut buf[..(left.min(1 << 20)) as usize])?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("{path}: the source ended early")));
        }
        f.write_all(&buf[..n])?;
        left -= n as u64;
    }
    f.flush()
}

pub fn write_bytes<T: ReadWriteSeek>(root: &Dir<T>, path: &str, data: &[u8]) -> io::Result<()> {
    write_file(root, path, data.len() as u64, &mut &data[..])
}

pub fn read_small<T: ReadWriteSeek>(root: &Dir<T>, path: &str, max: u64) -> io::Result<Vec<u8>> {
    let mut f = root.open_file(path)?;
    let mut v = Vec::new();
    (&mut f).take(max + 1).read_to_end(&mut v)?;
    if v.len() as u64 > max {
        return Err(io::Error::other(format!("{path} is larger than {max} bytes")));
    }
    Ok(v)
}

/// Removes `path` and everything below it (nothing when it does not exist).
pub fn remove_tree<T: ReadWriteSeek>(root: &Dir<T>, path: &str) -> io::Result<()> {
    let Ok(d) = root.open_dir(path) else {
        return match root.remove(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r,
        };
    };
    let names: Vec<(String, bool)> = d.iter().flatten().filter(|e| e.file_name() != "." && e.file_name() != "..").map(|e| (e.file_name(), e.is_dir())).collect();
    for (n, dir) in names {
        let p = format!("{path}/{n}");
        if dir { remove_tree(root, &p)? } else { root.remove(&p)? }
    }
    root.remove(path)
}

/// Every file below `path` with its size, `/`-separated relative to it.
pub fn walk<T: ReadWriteSeek>(root: &Dir<T>, path: &str, out: &mut Vec<(String, u64)>) -> io::Result<()> {
    fn go<T: ReadWriteSeek>(d: &Dir<T>, rel: &str, out: &mut Vec<(String, u64)>, depth: usize) -> io::Result<()> {
        if depth > 32 {
            return Err(io::Error::other("folders nested too deep"));
        }
        for e in d.iter() {
            let e = e?;
            let n = e.file_name();
            if n == "." || n == ".." {
                continue;
            }
            let r = if rel.is_empty() { n.clone() } else { format!("{rel}/{n}") };
            if e.is_dir() {
                go(&e.to_dir(), &r, out, depth + 1)?;
            } else {
                out.push((r, e.len()));
            }
        }
        Ok(())
    }
    go(&root.open_dir(path)?, "", out, 0)
}
