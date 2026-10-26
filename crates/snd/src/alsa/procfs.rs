//! `/proc/asound` as `linux-6.6/sound/core/{info.c,init.c,pcm.c,sound.c}` print it (aplay -l and the
//! card-name lookups read the control device; these files are for people and scripts).

use alloc::format;
use alloc::string::String;

/// `snd_card_info_read` (init.c): "%2i [%-15s]: %s - %s" and the long name.
fn cards() -> String {
    format!(" 0 [{:<15}]: {} - {}\n                      {}\n", super::CARD_ID, super::DRIVER, super::CARD_NAME, super::LONGNAME)
}

/// `snd_minor_info_read` (sound.c).
fn devices() -> String {
    format!("{:3}: [{:2}]   : control\n{:3}: [{:2}-{:2}]: digital audio playback\n{:3}: [{:2}-{:2}]: digital audio capture\n", super::CTL_MINOR, 0,
        super::PCM_MINOR, 0, 0, super::PCM_CAPTURE_MINOR, 0, 0)
}

/// `snd_pcm_proc_read` (pcm.c).
fn pcm() -> String {
    format!("00-00: {} : {} : playback 1 : capture 1\n", super::PCM_ID, super::PCM_NAME)
}

/// `snd_info_version_read` (info.c) with V2's uname release (syscall/src/info.rs RELEASE).
fn version() -> String {
    String::from("Advanced Linux Sound Architecture Driver Version k6.6.0-bitsos-v2.\n")
}

fn card0_id() -> String {
    format!("{}\n", super::CARD_ID)
}

vfs::procfile!("/proc/asound/cards", cards);
vfs::procfile!("/proc/asound/devices", devices);
vfs::procfile!("/proc/asound/pcm", pcm);
vfs::procfile!("/proc/asound/version", version);
vfs::procfile!("/proc/asound/card0/id", card0_id);
