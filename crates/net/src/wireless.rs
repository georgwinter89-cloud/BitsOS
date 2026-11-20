//! SYS-K: `/proc/net/wireless` (linux-7.3.1/net/wireless/wext-proc.c, values as `cfg80211_wireless_stats` in
//! wext-compat.c:1285 makes them for a station): the two header lines, then one line per wireless card of the
//! namespace. Associated with a signal: quality = signal clamped to -110..-40 plus 110, level = the signal in dBm,
//! both marked updated ('.'); the noise is not known (Linux's IW_QUAL_NOISE_INVALID shows -256). Not associated:
//! Linux's empty statistics (all 0, no marks). BitsOS keeps no failed-transmit or dropped-receive counts per
//! station: the discard columns stay 0.

use crate::ifview::Link;
use alloc::string::String;
use core::fmt::Write;
use devmodel::net::Wireless;

/// WIRELESS_EXT (include/uapi/linux/wireless.h).
const WE: u32 = 22;

/// One card's line.
pub fn line(name: &str, w: &Wireless) -> String {
    let (qual, level, noise, mark) = match w.signal_dbm.filter(|_| w.associated) {
        Some(sig) => (sig.clamp(-110, -40) + 110, sig, -256, '.'),
        None => (0, 0, 0, ' '),
    };
    let mut s = String::new();
    let _ = writeln!(s, "{name:>6}: {:04x}  {qual:3}{mark}  {level:3}{mark}  {noise:3}   {:6} {:6} {:6} {:6} {:6}   {:6}", 0, 0, 0, 0, 0, 0, 0);
    s
}

/// The file for the cards `wireless` names among `links` (a card that is not wireless is left out).
pub fn text(links: &[Link], wireless: impl Fn(&str) -> Option<Wireless>) -> String {
    let mut out = String::from("Inter-| sta-|   Quality        |   Discarded packets               | Missed | WE\n");
    let _ = writeln!(out, " face | tus | link level noise |  nwid  crypt   frag  retry   misc | beacon | {WE}");
    for l in links {
        if let Some(w) = wireless(&l.name) {
            out.push_str(&line(&l.name, &w));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_as_wext_proc_prints_them() {
        let up = Wireless { associated: true, signal_dbm: Some(-60) };
        assert_eq!(line("wlan0", &up), " wlan0: 0000   50.  -60.  -256        0      0      0      0      0        0\n");
        assert_eq!(line("wlan0", &Wireless { associated: true, signal_dbm: Some(-30) }).split_whitespace().nth(2), Some("70."), "quality tops at 70");
        assert_eq!(line("wlan0", &Wireless::default()), " wlan0: 0000    0     0     0        0      0      0      0      0        0\n");
    }
}
