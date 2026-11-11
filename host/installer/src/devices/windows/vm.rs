//! What QEMU mode asks Windows: the memory, the free space for the image, whether the Windows Hypervisor Platform
//! runs (WHPX), and an image file that is sparse. The platform's DLL exists only while the feature is on; it is
//! loaded from System32 alone and asked directly, so a feature that is switched on but waits for a restart counts
//! as off.

use super::ffi::wide;
use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::FreeLibrary;
use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

pub fn memory_bytes() -> Option<u64> {
    // SAFETY: a zeroed MEMORYSTATUSEX with its length set, as the call requires.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: `m` is a valid, writable MEMORYSTATUSEX.
    (unsafe { GlobalMemoryStatusEx(&mut m) } != 0).then_some(m.ullTotalPhys)
}

/// WHvGetCapability (WinHvPlatform.h).
type GetCapability = unsafe extern "system" fn(code: i32, buf: *mut c_void, size: u32, written: *mut u32) -> i32;
const CAPABILITY_HYPERVISOR_PRESENT: i32 = 0;

pub fn whpx_present() -> bool {
    let name = wide("WinHvPlatform.dll");
    // SAFETY: a NUL-terminated name; only System32 is searched.
    let lib = unsafe { LoadLibraryExW(name.as_ptr(), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if lib.is_null() {
        return false;
    }
    // SAFETY: `lib` is loaded; the name is NUL-terminated.
    let present = match unsafe { GetProcAddress(lib, c"WHvGetCapability".as_ptr().cast()) } {
        Some(f) => {
            // SAFETY: the export has WHvGetCapability's documented signature.
            let f: GetCapability = unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, GetCapability>(f) };
            let (mut yes, mut written) = (0i32, 0u32);
            // SAFETY: a 4-byte BOOL buffer for the "hypervisor present" capability.
            let hr = unsafe { f(CAPABILITY_HYPERVISOR_PRESENT, (&mut yes as *mut i32).cast(), 4, &mut written) };
            hr >= 0 && yes != 0
        }
        None => false,
    };
    // SAFETY: loaded above, freed once.
    unsafe { FreeLibrary(lib) };
    present
}

/// NTFS reserves the whole length of an ordinary file; a sparse one takes only what is written.
pub fn set_sparse(f: &std::fs::File) -> std::io::Result<()> {
    let mut got = 0u32;
    // SAFETY: an open file handle; FSCTL_SET_SPARSE without an input buffer sets the flag.
    let ok = unsafe { DeviceIoControl(f.as_raw_handle(), FSCTL_SET_SPARSE, null_mut(), 0, null_mut(), 0, &mut got, null_mut()) };
    if ok == 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

/// The bytes this user may still write in `dir`'s volume.
pub fn free_bytes(dir: &Path) -> Option<u64> {
    let w = wide(&dir.display().to_string());
    let mut free = 0u64;
    // SAFETY: a NUL-terminated path; only the first out-parameter is asked for.
    (unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut free, null_mut(), null_mut()) } != 0).then_some(free)
}
