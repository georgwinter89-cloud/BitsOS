//! The vendors' own archive formats, read just far enough to take single files out: a zip (Brave's release, stored
//! or deflate members, zip64 sizes) and a Debian package (an `ar` archive whose `data.tar.xz` holds the files).

use crate::sha::Sha256;
use miniz_oxide::inflate::stream::{InflateState, inflate};
use miniz_oxide::{DataFormat, MZFlush, MZStatus};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().expect("4 bytes"))
}
fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().expect("8 bytes"))
}

#[derive(Clone, Debug)]
pub struct ZipEntry {
    pub name: String,
    method: u16,
    comp: u64,
    pub size: u64,
    local: u64,
}

pub fn zip_list(f: &mut std::fs::File) -> io::Result<Vec<ZipEntry>> {
    let len = f.seek(SeekFrom::End(0))?;
    let tail = len.min(66_000);
    let mut t = vec![0u8; tail as usize];
    f.seek(SeekFrom::Start(len - tail))?;
    f.read_exact(&mut t)?;
    let eocd = (0..t.len().saturating_sub(21)).rev().find(|&i| u32le(&t, i) == 0x0605_4b50).ok_or_else(|| io::Error::other("zip: no end of central directory"))?;
    let (mut count, mut cd_size, mut cd_off) = (u16le(&t, eocd + 10) as u64, u32le(&t, eocd + 12) as u64, u32le(&t, eocd + 16) as u64);
    if cd_off == 0xFFFF_FFFF || count == 0xFFFF {
        // zip64: its locator sits right before the classic record.
        let loc = eocd.checked_sub(20).filter(|&l| u32le(&t, l) == 0x0706_4b50).ok_or_else(|| io::Error::other("zip: zip64 locator missing"))?;
        let mut z = [0u8; 56];
        f.seek(SeekFrom::Start(u64le(&t, loc + 8)))?;
        f.read_exact(&mut z)?;
        if u32le(&z, 0) != 0x0606_4b50 {
            return Err(io::Error::other("zip: bad zip64 record"));
        }
        (count, cd_size, cd_off) = (u64le(&z, 32), u64le(&z, 40), u64le(&z, 48));
    }
    if cd_size > 64 << 20 || count > 1_000_000 {
        return Err(io::Error::other("zip: central directory too large"));
    }
    let mut cd = vec![0u8; cd_size as usize];
    f.seek(SeekFrom::Start(cd_off))?;
    f.read_exact(&mut cd)?;
    let mut out = Vec::new();
    let mut i = 0usize;
    for _ in 0..count {
        if i + 46 > cd.len() || u32le(&cd, i) != 0x0201_4b50 {
            return Err(io::Error::other("zip: broken central directory"));
        }
        let (nl, el, cl) = (u16le(&cd, i + 28) as usize, u16le(&cd, i + 30) as usize, u16le(&cd, i + 32) as usize);
        let name = String::from_utf8_lossy(cd.get(i + 46..i + 46 + nl).ok_or_else(|| io::Error::other("zip: name past the end"))?).into_owned();
        let (mut comp, mut size, mut local) = (u32le(&cd, i + 20) as u64, u32le(&cd, i + 24) as u64, u32le(&cd, i + 42) as u64);
        let extra = cd.get(i + 46 + nl..i + 46 + nl + el).unwrap_or(&[]);
        let mut j = 0;
        while j + 4 <= extra.len() {
            let (id, l) = (u16le(extra, j), u16le(extra, j + 2) as usize);
            if id == 1 {
                let mut k = j + 4;
                for v in [&mut size, &mut comp, &mut local] {
                    if *v == 0xFFFF_FFFF && k + 8 <= extra.len() {
                        *v = u64le(extra, k);
                        k += 8;
                    }
                }
            }
            j += 4 + l;
        }
        out.push(ZipEntry { name, method: u16le(&cd, i + 10), comp, size, local });
        i += 46 + nl + el + cl;
    }
    Ok(out)
}

/// Copies one member to `out`, returning (SHA-256, bytes).
pub fn zip_extract(f: &mut std::fs::File, e: &ZipEntry, out: &mut dyn Write) -> io::Result<(String, u64)> {
    let mut h = [0u8; 30];
    f.seek(SeekFrom::Start(e.local))?;
    f.read_exact(&mut h)?;
    if u32le(&h, 0) != 0x0403_4b50 {
        return Err(io::Error::other(format!("zip: {}: bad local header", e.name)));
    }
    f.seek(SeekFrom::Current(u16le(&h, 26) as i64 + u16le(&h, 28) as i64))?;
    let (mut sha, mut count) = (Sha256::default(), 0u64);
    let mut data = (&mut *f).take(e.comp);
    match e.method {
        0 => {
            let mut buf = vec![0u8; 1 << 20];
            loop {
                let n = data.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                sha.update(&buf[..n]);
                count += n as u64;
                out.write_all(&buf[..n])?;
            }
        }
        8 => {
            let mut st = InflateState::new_boxed(DataFormat::Raw);
            let (mut inb, mut outb) = (vec![0u8; 1 << 16], vec![0u8; 1 << 18]);
            let (mut have, mut eof, mut done) = (0usize, false, false);
            while !done {
                if have < inb.len() && !eof {
                    let n = data.read(&mut inb[have..])?;
                    eof = n == 0;
                    have += n;
                }
                let r = inflate(&mut st, &inb[..have], &mut outb, if eof { MZFlush::Finish } else { MZFlush::None });
                inb.copy_within(r.bytes_consumed..have, 0);
                have -= r.bytes_consumed;
                sha.update(&outb[..r.bytes_written]);
                count += r.bytes_written as u64;
                out.write_all(&outb[..r.bytes_written])?;
                match r.status {
                    Ok(MZStatus::StreamEnd) => done = true,
                    Ok(_) if eof && r.bytes_written == 0 && r.bytes_consumed == 0 => return Err(io::Error::other(format!("zip: {}: deflate data ends early", e.name))),
                    Ok(_) => {}
                    // zlib's "buffer error": more input is coming, or the output was full (also after Finish).
                    Err(miniz_oxide::MZError::Buf) if !eof || r.bytes_written > 0 || r.bytes_consumed > 0 => {}
                    Err(x) => return Err(io::Error::other(format!("zip: {}: deflate error {x:?}", e.name))),
                }
            }
        }
        m => return Err(io::Error::other(format!("zip: {}: compression method {m} is not supported", e.name))),
    }
    if count != e.size {
        return Err(io::Error::other(format!("zip: {}: {count} bytes, the directory says {}", e.name, e.size)));
    }
    Ok((sha.hex(), count))
}

/// The members of an `ar` archive (a .deb): (name, bytes) for the ones `want` accepts.
pub fn ar_member(path: &Path, want: &dyn Fn(&str) -> bool) -> io::Result<Option<(String, Vec<u8>)>> {
    let mut f = std::fs::File::open(path)?;
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic)?;
    if &magic != b"!<arch>\n" {
        return Err(io::Error::other(format!("{}: not a Debian package", path.display())));
    }
    loop {
        let mut h = [0u8; 60];
        match f.read_exact(&mut h) {
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            r => r?,
        }
        let name = String::from_utf8_lossy(&h[..16]).trim().trim_end_matches('/').to_string();
        let size: u64 = String::from_utf8_lossy(&h[48..58]).trim().parse().map_err(|_| io::Error::other("ar: bad member size"))?;
        if want(&name) {
            if size > 256 << 20 {
                return Err(io::Error::other("ar: member too large"));
            }
            let mut v = vec![0u8; size as usize];
            f.read_exact(&mut v)?;
            return Ok(Some((name, v)));
        }
        f.seek(SeekFrom::Current((size + size % 2) as i64))?;
    }
}

/// `data.tar.xz` of a Debian package, decompressed.
pub fn deb_data_tar(path: &Path) -> Result<Vec<u8>, String> {
    let (name, xz) = ar_member(path, &|n| n.starts_with("data.tar")).map_err(|e| format!("{}: {e}", path.display()))?.ok_or_else(|| format!("{}: no data.tar member", path.display()))?;
    if name != "data.tar.xz" {
        return Err(format!("{}: {name} is not the expected data.tar.xz", path.display()));
    }
    let mut out = Vec::new();
    lzma_rs::xz_decompress(&mut io::BufReader::new(&xz[..]), &mut out).map_err(|e| format!("{}: data.tar.xz: {e:?}", path.display()))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_written_by_python_reads_back() {
        let d = std::env::temp_dir().join(format!("bi-zip-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let z = d.join("t.zip");
        let script = "import sys, zipfile\nwith zipfile.ZipFile(sys.argv[1], 'w') as z:\n z.writestr(zipfile.ZipInfo('a/stored'), b'plain bytes')\n z.writestr('a/deflated', b'x' * 300000 + bytes(range(256)) * 50, compress_type=zipfile.ZIP_DEFLATED)\n z.writestr('a/zeros', bytes(9000000), compress_type=zipfile.ZIP_DEFLATED)\n";
        let st = std::process::Command::new("python3").args(["-I", "-c", script]).arg(&z).status().expect("python3 for the zip fixture");
        assert!(st.success());
        let mut f = std::fs::File::open(&z).unwrap();
        let list = zip_list(&mut f).unwrap();
        assert_eq!(list.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["a/stored", "a/deflated", "a/zeros"]);
        let mut out = Vec::new();
        let (sha, n) = zip_extract(&mut f, &list[1], &mut out).unwrap();
        let want: Vec<u8> = [vec![b'x'; 300000], (0..=255u8).collect::<Vec<u8>>().repeat(50)].concat();
        assert_eq!((n, out.len()), (want.len() as u64, want.len()));
        assert!(out == want);
        assert_eq!(sha, crate::sha::hex(&want));
        let mut out = Vec::new();
        zip_extract(&mut f, &list[0], &mut out).unwrap();
        assert_eq!(out, b"plain bytes");
        // The input ends long before the output does (Brave's binary showed it): Finish with the output full.
        let mut out = Vec::new();
        assert_eq!(zip_extract(&mut f, &list[2], &mut out).unwrap().1, 9_000_000);
        assert!(out.iter().all(|&b| b == 0));
        let _ = std::fs::remove_dir_all(&d);
    }
}
