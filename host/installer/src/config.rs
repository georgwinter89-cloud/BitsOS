//! `\BITSOS2\CONFIG.TXT` from the answers. The installer never writes a guarded key (fs-fat's `GUARDED_KEYS`)
//! except `mcp=service`, and that only after the human said yes to remote control. Secrets stand only on their
//! own line (`wlan_psk=`, `token=`), never in a comment, and nothing here prints them.

/// fs-fat `protect::GUARDED_KEYS`: the keys only the person's own edit may set (README section 5).
pub const GUARDED: &[&str] = &["nvme_write", "nvme_data", "nvme_boot", "stick_write", "usb_msd_write", "mcp", "mcp_confirmer"];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Wifi {
    pub ssid: String,
    pub psk: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Answers {
    /// None: cable only.
    pub wifi: Option<Wifi>,
    /// The remote-control token, when the human said yes (then `mcp=service`).
    pub token: Option<String>,
    /// `screen=`; None leaves bitsboot's choice (the panel's native mode).
    pub screen: Option<String>,
    /// Test only (`--image`): lines a scenario needs, never guarded keys.
    pub extra: Vec<String>,
    /// An image for QEMU's window (`--qemu`): QEMU's network card, and the desktop, which on its own starts on real
    /// hardware only.
    pub qemu: bool,
}

/// CONFIG.TXT reads `#` as the start of a comment and trims spaces, so a value with either cannot be written.
fn plain(v: &str) -> bool {
    !v.is_empty() && v == v.trim() && !v.contains('#') && !v.chars().any(|c| c.is_control())
}

pub fn check_ssid(s: &str) -> Result<(), String> {
    if s.len() > 32 {
        return Err("the network name has more than 32 bytes".into());
    }
    if !plain(s) {
        return Err("the network name is empty, starts or ends with a space, or holds '#' or a control character (CONFIG.TXT cannot carry those)".into());
    }
    Ok(())
}

pub fn check_psk(p: &str) -> Result<(), String> {
    let hex = p.len() == 64 && p.bytes().all(|b| b.is_ascii_hexdigit());
    let ascii = (8..=63).contains(&p.len()) && p.bytes().all(|b| (0x20..0x7f).contains(&b));
    if !hex && !ascii {
        return Err("the Wi-Fi passphrase must be 8 to 63 printable ASCII characters (or the key as 64 hex digits)".into());
    }
    if !plain(p) {
        return Err("the Wi-Fi passphrase starts or ends with a space or holds '#': CONFIG.TXT cannot carry it".into());
    }
    Ok(())
}

pub fn check_screen(s: &str) -> Result<(), String> {
    if ["native", "largest", "firmware"].contains(&s) {
        return Ok(());
    }
    match s.split_once('x') {
        Some((w, h)) if !w.is_empty() && !h.is_empty() && w.len() <= 5 && h.len() <= 5 && w.bytes().chain(h.bytes()).all(|b| b.is_ascii_digit()) => Ok(()),
        _ => Err(format!("screen '{s}': give WxH (e.g. 1920x1080), native, largest or firmware")),
    }
}

fn key_of(line: &str) -> Option<&str> {
    let l = line.split('#').next().unwrap_or("").trim();
    l.split_once('=').map(|(k, _)| k.trim())
}

/// The guarded keys a CONFIG text sets (comments ignored, the reader's rules).
pub fn guarded_in(text: &str) -> Vec<String> {
    text.lines().filter_map(key_of).filter(|k| GUARDED.contains(k)).map(String::from).collect()
}

pub fn render(a: &Answers, version: &str) -> Result<String, String> {
    let mut s = format!(
        "# BitsOS CONFIG.TXT, written by bitsos-install {version}.\n\
         # name=value, '#' starts a comment, the last line of a name wins (README section 5 lists every key).\n\
         # The installer never sets nvme_write, nvme_data, nvme_boot, stick_write, usb_msd_write or mcp_confirmer:\n\
         # those stay your own edit. It sets mcp=service only when you said yes to remote control.\n"
    );
    match &a.wifi {
        Some(w) => {
            check_ssid(&w.ssid)?;
            check_psk(&w.psk)?;
            s.push_str("\n# Wi-Fi (the RTL8852BE; the firmware is in \\BITSOS2\\firmware\\rtw89\\).\nwlan=on\n");
            s.push_str(&format!("wlan_ssid={}\nwlan_psk={}\n", w.ssid, w.psk));
        }
        None if a.qemu => s.push_str("\n# Network: QEMU's virtio card; QEMU gives it an address by DHCP.\n"),
        None => s.push_str("\n# Network: cable only (Intel I226/82576 take an address by DHCP). Wi-Fi: wlan=on, wlan_ssid=, wlan_psk=.\n"),
    }
    match &a.token {
        Some(t) => {
            if !(8..=127).contains(&t.len()) || !t.bytes().all(|b| b.is_ascii_graphic() && b != b'#') {
                return Err("the remote-control token must be 8 to 127 printable characters".into());
            }
            s.push_str("\n# Remote control for an AI helper: the TLS service; a client is paired at this machine's keyboard.\n");
            s.push_str(&format!("token={t}\nmcp=service\n"));
        }
        None => s.push_str("\n# Remote control: off (no token).\n"),
    }
    if let Some(sc) = &a.screen {
        check_screen(sc)?;
        s.push_str(&format!("\n# Screen mode bitsboot sets (default: the panel's native mode).\nscreen={sc}\n"));
    }
    if a.qemu {
        s.push_str("\n# Made for QEMU's window (bitsos-install --qemu): the desktop starts there too.\ndesktop=on\n");
    }
    if !a.extra.is_empty() {
        s.push_str("\n# Test lines (--extra-config).\n");
        for l in &a.extra {
            s.push_str(l);
            s.push('\n');
        }
    }
    let allowed: &[&str] = if a.token.is_some() { &["mcp"] } else { &[] };
    if let Some(k) = guarded_in(&s).into_iter().find(|k| !allowed.contains(&k.as_str())) {
        return Err(format!("refused: CONFIG.TXT would set the guarded key {k}; only the person sets it, by hand"));
    }
    Ok(s)
}

/// A test line from `--extra-config`: one plain `key=value`, never a guarded key, never a secret's key.
pub fn check_extra(line: &str) -> Result<(), String> {
    let Some((k, v)) = line.split_once('=') else { return Err(format!("--extra-config '{line}': not key=value")) };
    let k = k.trim();
    if GUARDED.contains(&k) || ["token", "wlan_psk", "wlan_ssid", "claude_api_key", "wlan"].contains(&k) {
        return Err(format!("refused: --extra-config may not set {k}"));
    }
    if !k.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') || k.is_empty() || !plain(v.trim()) {
        return Err(format!("--extra-config '{line}': not a plain key=value line"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PSK: &str = "correct horse battery";
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn lines_with<'a>(s: &'a str, needle: &str) -> Vec<&'a str> {
        s.lines().filter(|l| !l.starts_with('#') && l.contains(needle)).collect()
    }

    #[test]
    fn the_guarded_keys_are_the_kernels() {
        assert_eq!(GUARDED, fs_fat::protect::GUARDED_KEYS, "a key the kernel guards and the installer does not know would pass render");
    }

    #[test]
    fn cable_only_and_no_remote_sets_no_guarded_key() {
        let s = render(&Answers::default(), "0.2.0").unwrap();
        assert!(guarded_in(&s).is_empty(), "{s}");
        assert!(!s.lines().any(|l| key_of(l).is_some()), "no setting at all:\n{s}");
    }

    #[test]
    fn mcp_only_with_the_yes_and_secrets_only_on_their_lines() {
        let a = Answers { wifi: Some(Wifi { ssid: "Home Net".into(), psk: PSK.into() }), token: Some(TOKEN.into()), screen: Some("1920x1080".into()), extra: vec![], qemu: false };
        let s = render(&a, "0.2.0").unwrap();
        assert_eq!(guarded_in(&s), ["mcp"]);
        assert_eq!(lines_with(&s, "mcp="), ["mcp=service"]);
        assert_eq!(lines_with(&s, PSK), [format!("wlan_psk={PSK}")]);
        assert_eq!(lines_with(&s, TOKEN), [format!("token={TOKEN}")]);
        assert!(!s.lines().filter(|l| l.starts_with('#')).any(|l| l.contains(PSK) || l.contains(TOKEN)), "no secret in a comment");
        assert!(s.contains("\nwlan=on\nwlan_ssid=Home Net\n") && s.contains("\nscreen=1920x1080\n"));
        for k in ["nvme_write", "nvme_data", "nvme_boot", "stick_write", "usb_msd_write", "mcp_confirmer"] {
            assert!(!s.lines().any(|l| key_of(l) == Some(k)), "{k}");
        }
    }

    #[test]
    fn values_the_file_cannot_carry_are_refused() {
        for (ssid, psk) in [("a#b", PSK), (" lead", PSK), ("ok", "short"), ("ok", "pass#word12"), ("ok", "tab\tinside1"), (&"x".repeat(33) as &str, PSK)] {
            let a = Answers { wifi: Some(Wifi { ssid: ssid.into(), psk: psk.into() }), ..Answers::default() };
            assert!(render(&a, "v").is_err(), "{ssid:?} {psk:?}");
        }
        assert!(render(&Answers { wifi: Some(Wifi { ssid: "n".into(), psk: "ab".repeat(32) }), ..Answers::default() }, "v").is_ok());
        assert!(render(&Answers { screen: Some("big".into()), ..Answers::default() }, "v").is_err());
    }

    #[test]
    fn extra_lines_never_carry_guarded_keys_or_secrets() {
        for bad in ["mcp=kernel", "nvme_write=on", "token=x", "wlan_psk=y", "mcp_confirmer=test", "x", "A=b", "k=a#b"] {
            assert!(check_extra(bad).is_err(), "{bad}");
        }
        check_extra("desktop=on").unwrap();
        // Even past the checks, render refuses a guarded key.
        let a = Answers { extra: vec!["stick_write=on".into()], ..Answers::default() };
        assert!(render(&a, "v").unwrap_err().contains("guarded"));
    }
}
