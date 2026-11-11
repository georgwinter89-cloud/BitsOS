//! QEMU on Windows: where Stefan Weil's QEMU for Windows (the build qemu.org points to) installs itself, the one
//! installer this program fetches when it is missing, and the shortcuts. Plain text in and out, tested on any host.

use std::path::PathBuf;

/// A download pinned by size and hash.
pub struct Pinned {
    pub version: &'static str,
    pub url: &'static str,
    pub size: u64,
    /// The project publishes SHA-512 sums only (`<file>.sha512` beside each installer).
    pub sha512: &'static str,
}

/// To move to a newer QEMU: take the newest `qemu-w64-setup-<date>.exe` from https://qemu.weilnetz.de/w64/<year>/
/// (the year folders keep every build; the top level only the latest), its size from the listing (or curl -I's
/// Content-Length), and the hash from the `.sha512` file beside it; change all four fields together, then run
/// `cargo test -p bitsos-install` (it checks their form) and try the download once on Windows.
pub const QEMU_FOR_WINDOWS: Pinned = Pinned {
    version: "20260811",
    url: "https://qemu.weilnetz.de/w64/2026/qemu-w64-setup-20260811.exe",
    size: 206_615_928,
    sha512: "5bcf9eed634e8575a37b74f445af41a2fe4106da512d0c30c368301d4c105037fdfab40a5287367a28a957624cddebbc8c07e16c88ab6634f554cdf3d16bf543",
};

impl Pinned {
    pub fn file_name(&self) -> &'static str {
        self.url.rsplit('/').next().unwrap_or(self.url)
    }
}

/// The folder in `reg query HKLM\SOFTWARE\QEMU /v Install_Dir`'s answer.
pub fn parse_reg_install_dir(out: &str) -> Option<String> {
    out.lines().find_map(|l| {
        let l = l.trim();
        let rest = l.strip_prefix("Install_Dir")?.trim_start();
        let v = rest.strip_prefix("REG_SZ")?.trim();
        (!v.is_empty()).then(|| v.to_string())
    })
}

/// A Windows path below `dir` (backslashes whatever the host the tests run on).
pub fn join(dir: &str, rest: &str) -> String {
    format!("{}\\{rest}", dir.trim_end_matches(['\\', '/']))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Install {
    pub exe: PathBuf,
    pub code: PathBuf,
    pub vars: PathBuf,
}

/// QEMU's files in an install folder: the program and the edk2 firmware it ships (x86-64 code, the variables
/// template QEMU names after i386 because both use it).
pub fn install_in(dir: &str) -> Install {
    Install { exe: join(dir, "qemu-system-x86_64.exe").into(), code: join(dir, "share\\edk2-x86_64-code.fd").into(), vars: join(dir, "share\\edk2-i386-vars.fd").into() }
}

/// Where to look: the folder its installer wrote to the registry, then the Program Files folders.
pub fn candidates(reg: Option<String>, program_files: &[Option<String>]) -> Vec<String> {
    let mut v: Vec<String> = reg.into_iter().collect();
    for p in program_files.iter().flatten() {
        let d = join(p, "qemu");
        if !v.iter().any(|x| x.eq_ignore_ascii_case(&d)) {
            v.push(d);
        }
    }
    v
}

/// PowerShell ends a single-quoted string at any of these, not only at `'`; each is doubled to stay a letter.
const PS_QUOTES: [char; 5] = ['\'', '\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'];

/// A single-quoted PowerShell string.
pub fn ps_quote(s: &str) -> String {
    let mut q = String::from("'");
    for c in s.chars() {
        if PS_QUOTES.contains(&c) {
            q.push(c);
        }
        q.push(c);
    }
    q.push('\'');
    q
}

/// The shortcut's description names the script it starts: `--uninstall` keeps a shortcut that another image
/// folder's run has since written over this one's.
pub fn shortcut_description(target: &str) -> String {
    format!("BitsOS in a window (QEMU): {target}")
}

/// A shortcut file starts `target`: Windows keeps the description as UTF-16, the target's path also as plain bytes
/// when it is ASCII.
pub fn shortcut_names(lnk: &[u8], target: &str) -> bool {
    let wide: Vec<u8> = shortcut_description(target).encode_utf16().flat_map(u16::to_le_bytes).collect();
    let has = |n: &[u8]| !n.is_empty() && lnk.windows(n.len()).any(|w| w == n);
    has(&wide) || (target.is_ascii() && has(target.as_bytes()))
}

/// The PowerShell that puts `BitsOS.lnk` on the Desktop and into the Start menu (wherever Windows has those
/// folders, OneDrive included) and prints each shortcut's path.
pub fn shortcut_script(target: &str, workdir: &str, icon: &str) -> String {
    format!(
        "$ErrorActionPreference = 'Stop'\n\
         $w = New-Object -ComObject WScript.Shell\n\
         foreach ($d in @([Environment]::GetFolderPath('Desktop'), [Environment]::GetFolderPath('Programs'))) {{\n\
         \x20 if (-not $d) {{ continue }}\n\
         \x20 $p = Join-Path $d 'BitsOS.lnk'\n\
         \x20 $s = $w.CreateShortcut($p)\n\
         \x20 $s.TargetPath = {}\n\
         \x20 $s.WorkingDirectory = {}\n\
         \x20 $s.IconLocation = {}\n\
         \x20 $s.Description = {}\n\
         \x20 $s.WindowStyle = 7\n\
         \x20 $s.Save()\n\
         \x20 Write-Output $p\n\
         }}\n",
        ps_quote(target),
        ps_quote(workdir),
        ps_quote(&format!("{icon},0")),
        ps_quote(&shortcut_description(target))
    )
}

/// Starts the QEMU installer silently with administrator rights (Windows shows its permission prompt) and waits.
pub fn installer_script(exe: &str) -> String {
    format!("$p = Start-Process -FilePath {} -ArgumentList '/S' -Verb RunAs -Wait -PassThru\nexit $p.ExitCode\n", ps_quote(exe))
}
