//! Thin, safe wrappers around the Win32 calls the disk code needs. Every answer comes back as plain bytes or
//! strings; `winparse` reads the structures, so nothing here casts a buffer to a Windows struct.

use std::io;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_MORE_DATA, ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, FlushFileBuffers, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
    GetVolumePathNamesForVolumeNameW, OPEN_EXISTING, QueryDosDeviceW, ReadFile, SetFilePointerEx, WriteFile, FILE_BEGIN,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_MULTI_SZ, RRF_RT_REG_SZ, RegGetValueW};
use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;

/// A kernel handle, closed when dropped.
pub struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateFileW and is closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn from_wide(w: &[u16]) -> String {
    String::from_utf16_lossy(&w[..w.iter().position(|c| *c == 0).unwrap_or(w.len())])
}

fn check(ok: i32) -> io::Result<()> {
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// Opens a file or device (`\\.\PhysicalDrive2`, `\\?\Volume{...}`) that already exists, shared for reading and
/// writing. `access` 0 asks for nothing but queries, which needs no administrator rights.
pub fn open(path: &str, access: u32, flags: u32) -> io::Result<Handle> {
    let w = wide(path);
    // SAFETY: `w` is NUL-terminated and outlives the call; no security attributes, no template.
    let h = unsafe { CreateFileW(w.as_ptr(), access, FILE_SHARE_READ | FILE_SHARE_WRITE, null(), OPEN_EXISTING, flags, null_mut()) };
    if h == INVALID_HANDLE_VALUE { Err(io::Error::last_os_error()) } else { Ok(Handle(h)) }
}

/// A synchronous DeviceIoControl; returns the bytes the driver filled in.
pub fn ioctl(h: &Handle, code: u32, input: &[u8], out_len: usize) -> io::Result<Vec<u8>> {
    let mut out = vec![0u8; out_len];
    let mut got = 0u32;
    let inp = if input.is_empty() { null() } else { input.as_ptr().cast() };
    let outp = if out.is_empty() { null_mut() } else { out.as_mut_ptr().cast() };
    // SAFETY: both buffers are valid for the lengths given; the call is synchronous (no OVERLAPPED).
    check(unsafe { DeviceIoControl(h.0, code, inp, input.len() as u32, outp, out_len as u32, &mut got, null_mut()) })?;
    out.truncate(got as usize);
    Ok(out)
}

fn seek(h: &Handle, off: u64) -> io::Result<()> {
    // SAFETY: plain call on an open handle.
    check(unsafe { SetFilePointerEx(h.0, off as i64, null_mut(), FILE_BEGIN) })
}

/// Reads exactly `buf.len()` bytes at `off`.
pub fn read_at(h: &Handle, off: u64, buf: &mut [u8]) -> io::Result<()> {
    seek(h, off)?;
    let mut done = 0;
    while done < buf.len() {
        let mut got = 0u32;
        let n = (buf.len() - done).min(1 << 30) as u32;
        // SAFETY: the slice is valid for `n` bytes from `done`; synchronous handle.
        check(unsafe { ReadFile(h.0, buf[done..].as_mut_ptr(), n, &mut got, null_mut()) })?;
        if got == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the disk ended early"));
        }
        done += got as usize;
    }
    Ok(())
}

/// Writes all of `buf` at `off`.
pub fn write_at(h: &Handle, off: u64, buf: &[u8]) -> io::Result<()> {
    seek(h, off)?;
    let mut done = 0;
    while done < buf.len() {
        let mut put = 0u32;
        let n = (buf.len() - done).min(1 << 30) as u32;
        // SAFETY: the slice is valid for `n` bytes from `done`; synchronous handle.
        check(unsafe { WriteFile(h.0, buf[done..].as_ptr(), n, &mut put, null_mut()) })?;
        if put == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "the disk took no bytes"));
        }
        done += put as usize;
    }
    Ok(())
}

pub fn flush(h: &Handle) -> io::Result<()> {
    // SAFETY: plain call on an open handle.
    check(unsafe { FlushFileBuffers(h.0) })
}

struct Find(HANDLE);

impl Drop for Find {
    fn drop(&mut self) {
        // SAFETY: the handle came from FindFirstVolumeW and is closed once.
        unsafe { FindVolumeClose(self.0) };
    }
}

/// Every volume's name (`\\?\Volume{...}\`).
pub fn volumes() -> io::Result<Vec<String>> {
    let mut buf = [0u16; 1024];
    // SAFETY: the buffer is valid for its length.
    let h = unsafe { FindFirstVolumeW(buf.as_mut_ptr(), buf.len() as u32) };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let f = Find(h);
    let mut out = vec![from_wide(&buf)];
    loop {
        // SAFETY: as above, on the search handle.
        if unsafe { FindNextVolumeW(f.0, buf.as_mut_ptr(), buf.len() as u32) } == 0 {
            let e = io::Error::last_os_error();
            return if e.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) { Ok(out) } else { Err(e) };
        }
        out.push(from_wide(&buf));
    }
}

/// Where a volume is mounted, as the raw multi-string (drive letters and folders).
pub fn mount_paths(volume: &str) -> io::Result<Vec<u16>> {
    let w = wide(volume);
    let mut len = 256u32;
    loop {
        let mut buf = vec![0u16; len as usize];
        // SAFETY: the buffer is valid for `len` characters; `len` receives the size needed.
        if unsafe { GetVolumePathNamesForVolumeNameW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32, &mut len) } != 0 {
            return Ok(buf);
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(ERROR_MORE_DATA as i32) || len as usize <= buf.len() {
            return Err(e);
        }
    }
}

/// The NT device of a volume (`\Device\HarddiskVolume3`).
pub fn nt_device(volume: &str) -> Option<String> {
    let name = volume.strip_prefix("\\\\?\\")?.trim_end_matches('\\');
    let w = wide(name);
    let mut buf = [0u16; 512];
    // SAFETY: `w` is NUL-terminated, the buffer valid for its length.
    let n = unsafe { QueryDosDeviceW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    (n > 0).then(|| from_wide(&buf))
}

/// The volume (`\\?\Volume{...}\`) mounted at a root such as `C:\`.
pub fn volume_at(root: &str) -> io::Result<String> {
    let w = wide(root);
    let mut buf = [0u16; 128];
    // SAFETY: `w` is NUL-terminated, the buffer valid for its length.
    check(unsafe { GetVolumeNameForVolumeMountPointW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) })?;
    Ok(from_wide(&buf))
}

/// The Windows folder (`C:\Windows`) and the root of the volume it is on (`C:\`).
pub fn windows_root() -> io::Result<(String, String)> {
    let mut dir = [0u16; 512];
    // SAFETY: the buffer is valid for its length.
    let n = unsafe { GetWindowsDirectoryW(dir.as_mut_ptr(), dir.len() as u32) };
    if n == 0 || n as usize >= dir.len() {
        return Err(io::Error::last_os_error());
    }
    let mut root = [0u16; 512];
    // SAFETY: `dir` is NUL-terminated by the call above, the buffer valid for its length.
    check(unsafe { GetVolumePathNameW(dir.as_ptr(), root.as_mut_ptr(), root.len() as u32) })?;
    Ok((from_wide(&dir), from_wide(&root)))
}

/// A string (or multi-string) value under HKEY_LOCAL_MACHINE, as raw UTF-16.
pub fn registry(key: &str, value: &str, multi: bool) -> Option<Vec<u16>> {
    let (k, v) = (wide(key), wide(value));
    let flags = if multi { RRF_RT_REG_MULTI_SZ } else { RRF_RT_REG_SZ };
    let mut bytes = 0u32;
    // SAFETY: a size query (no data buffer); the strings are NUL-terminated.
    if unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, k.as_ptr(), v.as_ptr(), flags, null_mut(), null_mut(), &mut bytes) } != 0 {
        return None;
    }
    let mut buf = vec![0u16; bytes as usize / 2 + 2];
    let mut bytes = (buf.len() * 2) as u32;
    // SAFETY: the buffer is valid for `bytes` bytes.
    (unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, k.as_ptr(), v.as_ptr(), flags, null_mut(), buf.as_mut_ptr().cast(), &mut bytes) } == 0).then_some(buf)
}
