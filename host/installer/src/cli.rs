//! Command-line options. Without `--erase`/`--update` the installer asks; with one of them every answer is a flag
//! (for an AI agent), but the disk id still has to be the one the person read in the list.

use std::path::PathBuf;

pub const HELP: &str = "usage: bitsos-install [options]
Puts BitsOS onto a USB stick. Without options it shows the sticks and asks everything.

  --list                       show the USB sticks this installer would offer, then stop
  --erase <id>                 erase the stick <id> and install (no questions; all answers as flags)
  --update <id>                add this release to the BitsOS stick <id> as a new generation; its settings,
                               data and older generations stay (no questions)
      <id> is the word after [n] on the stick's line in --list (sdb-31.0G-3fa2 on Linux, disk2-31.0G-3fa2 on
      Windows; the last part is a fingerprint of the stick's vendor, model and serial, so the id fits that one
      stick only). A stick without a serial number is chosen interactively. An AI agent must show the person
      that list and use the id the person confirms; it never picks a disk by itself. Writing needs root on
      Linux (sudo) and an administrator terminal on Windows.
  --wifi <name>                Wi-Fi network name, with --wifi-psk-file <file> (the passphrase on its first line)
  --cable-only                 no Wi-Fi
  --remote yes|no              remote control for an AI helper over the network (TLS, paired at the keyboard);
                               in this mode the token is not printed, only where it is saved
  --screen auto|<W>x<H>|native|largest|firmware   screen mode (default auto: 2560x1440 if the screen offers
                               it, else the firmware's mode; largest: the biggest mode it offers)
  --trees all|none|<a,b,...>   program trees to install (default all the release has)
  --steam yes|no               fetch Valve's Steam client on this computer and add it
  --brave yes|no               fetch the Brave browser on this computer and add it
  --allow-missing              go on when a tree asked for (Steam, Brave) cannot be fetched; without it the
                               run stops before writing
  --assets <dir>               take the release files from <dir> instead of downloading them
  --manifest-sha256 <hex>      with --assets, for an installer built without a release: the manifest's hash
  --vendor-cache <dir>         a folder that already holds Steam's or Brave's downloads (repeatable)
  --offline                    download nothing
  --host-dir <dir>             where the stick's ID and token are kept (default ~/.config/bitsos or %APPDATA%\\BitsOS)
  --cache <dir>                where downloads are kept (default ~/.cache or %LOCALAPPDATA%, bitsos-install)
  --qemu                       try BitsOS in a window instead: an image file and QEMU, no stick (--qemu --help)
  --help";

pub const QEMU_HELP: &str = "usage: bitsos-install --qemu [options]
Try BitsOS in a window (QEMU). Touches no disk and no stick: it writes an image file, a start script and a
shortcut, and sets QEMU up if this computer lacks it. Run again to add a newer release as a new generation
(CONFIG.TXT and data.img stay). Without options it asks; with --remote and --start it asks nothing.

  --remote yes|no              remote control for an AI helper (QEMU forwards a port on 127.0.0.1 to it)
  --start yes|no               start BitsOS once at the end
  --install-qemu               QEMU or its UEFI firmware is missing: install it without asking (Linux: the
                               distribution's package manager through sudo or pkexec; Windows: the pinned QEMU
                               installer from qemu.weilnetz.de, checked by SHA-512, with its own UAC prompt).
                               Without it a run with every answer as a flag stops and names the command
  --image-dir <dir>            where the image and its files go (default $XDG_DATA_HOME/bitsos or
                               ~/.local/share/bitsos; %LOCALAPPDATA%\\BitsOS on Windows)
  --size <n>G                  size of a new image (default 16G; sparse: it takes only what is written)
  --screen <W>x<H>|firmware    the window's screen mode (default 1600x900)
  --trees none|<a,b,...>       program trees (default none: the system and the desktop); every tree is
                               untested in QEMU
  --steam yes|no, --brave yes|no   Steam or Brave from their makers (untested in QEMU, default no)
  --assets <dir>, --manifest-sha256 <hex>, --vendor-cache <dir>, --offline, --cache <dir>, --allow-missing
                               as for a stick
  --dry-run                    show what would be done and the QEMU command; write and install nothing
                               (with --uninstall: list the files, remove nothing)
  --uninstall                  remove the image, its firmware variables, the start script and the shortcuts
                               this installer made (listed first; asks, or --yes)
  --yes                        with --uninstall: no question
  --help";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Net {
    Ask,
    Cable,
    Wifi { ssid: String, psk_file: PathBuf },
}

#[derive(Clone, Debug)]
pub struct Opts {
    pub list: bool,
    pub erase: Option<String>,
    pub update: Option<String>,
    pub net: Net,
    pub remote: Option<bool>,
    pub screen: Option<String>,
    pub trees: Option<String>,
    pub steam: Option<bool>,
    pub brave: Option<bool>,
    pub assets: Option<PathBuf>,
    pub manifest_sha256: Option<String>,
    pub vendor_cache: Vec<PathBuf>,
    pub offline: bool,
    pub host_dir: Option<PathBuf>,
    pub cache: Option<PathBuf>,
    /// Test only: an image file instead of a disk.
    pub image: Option<PathBuf>,
    pub image_size: Option<u64>,
    pub extra_config: Vec<String>,
    pub allow_missing: bool,
    pub qemu: Option<QemuOpts>,
}

/// `--qemu`: an image file and a QEMU window instead of a stick.
#[derive(Clone, Debug, Default)]
pub struct QemuOpts {
    pub install_qemu: bool,
    pub image_dir: Option<PathBuf>,
    pub size: Option<u64>,
    pub start: Option<bool>,
    pub uninstall: bool,
    pub yes: bool,
    pub dry_run: bool,
}

fn yes_no(flag: &str, v: &str) -> Result<bool, String> {
    match v {
        "yes" => Ok(true),
        "no" => Ok(false),
        _ => Err(format!("{flag} takes yes or no")),
    }
}

/// `8G`, `512M`, or bytes.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let (n, mul) = match s.as_bytes().last() {
        Some(b'G') => (&s[..s.len() - 1], 1u64 << 30),
        Some(b'M') => (&s[..s.len() - 1], 1 << 20),
        _ => (s, 1),
    };
    n.parse::<u64>().ok().and_then(|v| v.checked_mul(mul)).ok_or_else(|| format!("bad size '{s}'"))
}

impl Opts {
    pub fn parse(args: Vec<String>) -> Result<Option<Opts>, String> {
        let mut o = Opts {
            list: false,
            erase: None,
            update: None,
            net: Net::Ask,
            remote: None,
            screen: None,
            trees: None,
            steam: None,
            brave: None,
            assets: None,
            manifest_sha256: None,
            vendor_cache: Vec::new(),
            offline: false,
            host_dir: None,
            cache: None,
            image: None,
            image_size: None,
            extra_config: Vec::new(),
            allow_missing: false,
            qemu: None,
        };
        let mut q = QemuOpts::default();
        let (mut qemu, mut qemu_only) = (false, None::<String>);
        let (mut wifi, mut psk, mut cable) = (None, None, false);
        let mut it = args.into_iter();
        while let Some(a) = it.next() {
            let (flag, inline) = match a.split_once('=') {
                Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
                _ => (a.clone(), None),
            };
            let mut val = || inline.clone().or_else(|| it.next()).ok_or_else(|| format!("{flag} needs a value"));
            match flag.as_str() {
                "--help" | "-h" => return Ok(None),
                "--list" => o.list = true,
                "--erase" => o.erase = Some(val()?),
                "--update" => o.update = Some(val()?),
                "--wifi" => wifi = Some(val()?),
                "--wifi-psk-file" => psk = Some(PathBuf::from(val()?)),
                "--cable-only" => cable = true,
                "--remote" => o.remote = Some(yes_no("--remote", &val()?)?),
                "--screen" => o.screen = Some(val()?),
                "--trees" => o.trees = Some(val()?),
                "--steam" => o.steam = Some(yes_no("--steam", &val()?)?),
                "--brave" => o.brave = Some(yes_no("--brave", &val()?)?),
                "--assets" => o.assets = Some(PathBuf::from(val()?)),
                "--manifest-sha256" => o.manifest_sha256 = Some(val()?),
                "--vendor-cache" => o.vendor_cache.push(PathBuf::from(val()?)),
                "--offline" => o.offline = true,
                "--allow-missing" => o.allow_missing = true,
                "--host-dir" => o.host_dir = Some(PathBuf::from(val()?)),
                "--cache" => o.cache = Some(PathBuf::from(val()?)),
                "--image" => o.image = Some(PathBuf::from(val()?)),
                "--image-size" => o.image_size = Some(parse_size(&val()?)?),
                "--extra-config" => o.extra_config.extend(val()?.split(';').map(str::trim).filter(|s| !s.is_empty()).map(String::from)),
                "--qemu" => qemu = true,
                "--install-qemu" | "--image-dir" | "--size" | "--start" | "--uninstall" | "--yes" | "--dry-run" => {
                    qemu_only.get_or_insert(flag.clone());
                    match flag.as_str() {
                        "--install-qemu" => q.install_qemu = true,
                        "--image-dir" => q.image_dir = Some(PathBuf::from(val()?)),
                        "--size" => q.size = Some(parse_size(&val()?)?),
                        "--start" => q.start = Some(yes_no("--start", &val()?)?),
                        "--uninstall" => q.uninstall = true,
                        "--yes" => q.yes = true,
                        _ => q.dry_run = true,
                    }
                }
                "--wifi-psk" | "--token" => return Err(format!("{flag}: secrets never go on the command line (other users can read it); use --wifi-psk-file")),
                _ => return Err(format!("unknown option '{a}'\n{HELP}")),
            }
        }
        o.net = match (wifi, psk, cable) {
            (None, None, false) => Net::Ask,
            (None, None, true) => Net::Cable,
            (Some(ssid), Some(psk_file), false) => Net::Wifi { ssid, psk_file },
            (Some(_), None, false) => return Err("--wifi needs --wifi-psk-file".into()),
            _ => return Err("give --wifi <name> --wifi-psk-file <file>, or --cable-only".into()),
        };
        if let Some(f) = qemu_only.filter(|_| !qemu) {
            return Err(format!("{f} belongs to --qemu"));
        }
        if qemu {
            let stick = [(o.list, "--list"), (o.erase.is_some(), "--erase"), (o.update.is_some(), "--update"), (o.image.is_some(), "--image"), (o.net != Net::Ask, "--wifi/--cable-only"), (!o.extra_config.is_empty(), "--extra-config")];
            if let Some((_, f)) = stick.iter().find(|(set, _)| *set) {
                return Err(format!("{f} is for a stick; --qemu writes an image file of its own"));
            }
            if q.yes && !q.uninstall {
                return Err("--yes belongs to --uninstall".into());
            }
            if q.size.is_some_and(|s| s < 8 << 30) {
                return Err("--size: at least 8G".into());
            }
            o.qemu = Some(q);
        }
        if o.erase.is_some() && o.update.is_some() {
            return Err("--erase and --update exclude each other".into());
        }
        if !o.extra_config.is_empty() && o.image.is_none() {
            return Err("--extra-config is for test images only (--image)".into());
        }
        if o.image_size.is_some() && o.image.is_none() {
            return Err("--image-size belongs to --image".into());
        }
        for l in &o.extra_config {
            crate::config::check_extra(l)?;
        }
        if let Some(s) = &o.screen
            && s != "auto"
        {
            crate::config::check_screen(s)?;
        }
        Ok(Some(o))
    }

    /// Every answer is a flag: nothing is asked (an AI agent's run).
    pub fn batch(&self) -> bool {
        self.erase.is_some() || self.update.is_some() || self.qemu.as_ref().is_some_and(|q| q.dry_run || q.uninstall && q.yes || self.remote.is_some() && q.start.is_some())
    }

    /// In batch mode, the answers that are still missing.
    pub fn missing(&self) -> Vec<&'static str> {
        let mut m = Vec::new();
        if self.erase.is_some() {
            if self.net == Net::Ask {
                m.push("--wifi <name> --wifi-psk-file <file> | --cable-only");
            }
            if self.remote.is_none() {
                m.push("--remote yes|no");
            }
        }
        if self.steam.is_none() {
            m.push("--steam yes|no");
        }
        if self.brave.is_none() {
            m.push("--brave yes|no");
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(a: &str) -> Result<Option<Opts>, String> {
        Opts::parse(a.split_whitespace().map(String::from).collect())
    }

    #[test]
    fn batch_needs_every_answer() {
        let o = p("--erase sdb-31.0G --cable-only --remote no --steam no --brave yes").unwrap().unwrap();
        assert!(o.batch() && o.missing().is_empty());
        let o = p("--erase sdb-31.0G --remote no").unwrap().unwrap();
        assert_eq!(o.missing().len(), 3);
        assert!(p("--update sdb-31.0G --steam no --brave no").unwrap().unwrap().missing().is_empty());
    }

    #[test]
    fn secrets_and_test_flags_are_kept_in_their_place() {
        assert!(p("--wifi-psk hunter22").is_err());
        assert!(p("--wifi Net").is_err());
        assert!(p("--extra-config desktop=on").is_err(), "only with --image");
        assert!(p("--image a.img --extra-config mcp=kernel").is_err());
        assert!(p("--erase a --update b").is_err());
        assert_eq!(parse_size("8G").unwrap(), 8 << 30);
        assert!(p("--screen 1920x1080").is_ok() && p("--screen huge").is_err());
    }
}
