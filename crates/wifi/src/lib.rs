//! The 802.11 station layer of BitsOS, independent of the chip (spec/wlan.md §5.3, §5.4): the
//! scan list, the join (open authentication, association, the WPA2-PSK 4-way and group key
//! handshakes with the KRACK guard), the RSN element, the receive checks (CCMP PN replay per TID,
//! duplicates, the control port) and the supervisor that joins again with back-off. Shared by
//! every WLAN driver: rtw89 (RTL8852BE) first; a later Intel driver reuses it.
//!
//! No I/O and no unsafe code: the driver sends what an action asks for and feeds what came in;
//! time comes in as milliseconds of the caller's clock. Events go to typed logs the driver drains
//! into klog (B6). Key material lives in wiped [`secret::Secret`]s and never reaches an event, a
//! `Debug` print or an error (R8i). Ported from the JS driver's sta-link.mjs / sta-mlme.mjs /
//! wpa.mjs (WZ:js) and V1's wlan/eapol*.asm, with mac80211 and wpa_supplicant as reference.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod crypto;
pub mod eapol;
pub mod events;
pub mod mlme;
#[cfg(any(test, feature = "peer"))]
pub mod peer;
pub mod replay;
pub mod rsn;
pub mod scan;
pub mod secret;
pub mod supervisor;

pub use events::WifiEvent;
pub use mlme::{Action, JoinFailure, JoinTiming, Mgmt, Mlme, Phase};
pub use scan::{Band, Bss, Mac, ScanList};
pub use supervisor::Supervisor;
