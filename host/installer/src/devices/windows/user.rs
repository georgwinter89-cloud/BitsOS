//! The person running the installer: administrator rights (a whole disk needs them), their own folders' access
//! list (the token and stick id are for them alone), and a console that does not echo a passphrase.

use std::io;
use std::path::Path;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetTokenInformation, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SetFileSecurityW, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER,
    TokenElevation, TokenUser,
};
use windows_sys::Win32::System::Console::{ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::ffi::wide;

struct Token(HANDLE);

impl Drop for Token {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenProcessToken and is closed once.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

fn token() -> io::Result<Token> {
    let mut h: HANDLE = null_mut();
    // SAFETY: the pseudo-handle of this process; `h` receives the token.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut h) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Token(h))
}

/// This process runs with administrator rights (elevated).
pub fn elevated() -> bool {
    let Ok(t) = token() else { return false };
    let mut e = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut got = 0u32;
    // SAFETY: `e` is the structure TokenElevation fills, of the size given.
    let ok = unsafe { GetTokenInformation(t.0, TokenElevation, (&mut e as *mut TOKEN_ELEVATION).cast(), size_of::<TOKEN_ELEVATION>() as u32, &mut got) };
    ok != 0 && e.TokenIsElevated != 0
}

/// The user's SID as text (`S-1-5-21-...`).
fn user_sid() -> io::Result<String> {
    let t = token()?;
    // u64 words: TOKEN_USER holds a pointer and must be aligned for it.
    let mut buf = vec![0u64; 64];
    let mut got = 0u32;
    // SAFETY: the buffer is valid and aligned for its byte length.
    if unsafe { GetTokenInformation(t.0, TokenUser, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &mut got) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call succeeded, so the buffer starts with a TOKEN_USER whose SID points into it.
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut s: *mut u16 = null_mut();
    // SAFETY: `sid` is valid while `buf` lives; `s` receives a LocalAlloc'd string.
    if unsafe { ConvertSidToStringSidW(sid, &mut s) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut n = 0;
    // SAFETY: `s` is NUL-terminated; it is freed once after reading.
    let text = unsafe {
        while *s.add(n) != 0 {
            n += 1;
        }
        let t = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
        LocalFree(s.cast());
        t
    };
    Ok(text)
}

/// Leaves `p` (a folder or file) accessible to the current user alone: one entry, full control, inherited by what
/// is created inside, nothing inherited from above.
pub fn restrict_to_user(p: &Path) -> io::Result<()> {
    let sddl = wide(&format!("D:P(A;OICI;FA;;;{})", user_sid()?));
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: `sddl` is NUL-terminated; `sd` receives a LocalAlloc'd descriptor.
    if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let path = wide(&p.to_string_lossy());
    // SAFETY: `path` is NUL-terminated and `sd` a valid descriptor, freed once afterwards.
    let ok = unsafe { SetFileSecurityW(path.as_ptr(), DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION, sd) };
    let err = io::Error::last_os_error();
    // SAFETY: allocated by the conversion above.
    unsafe { LocalFree(sd.cast()) };
    if ok == 0 { Err(err) } else { Ok(()) }
}

/// Turns the console's echo of typed characters on or off (no console: nothing to do).
pub fn echo(on: bool) {
    // SAFETY: plain calls on the standard input handle; `mode` receives its mode.
    unsafe {
        let h = GetStdHandle(STD_INPUT_HANDLE);
        let mut mode = 0u32;
        if GetConsoleMode(h, &mut mode) != 0 {
            SetConsoleMode(h, if on { mode | ENABLE_ECHO_INPUT } else { mode & !ENABLE_ECHO_INPUT });
        }
    }
}
