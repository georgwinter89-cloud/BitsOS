//! Where the read-only reference trees lie (W22, C1): `$HOME/BitsOS/<rel>`, the desk's layout next to the V2
//! checkout, so no home value is written into the public tree. WLAN_TOOLS and the other variables override it.
//! Shared by the integration tests here and, through `#[path]`, by the crate's own unit tests.

use std::path::PathBuf;

/// `$HOME/BitsOS/<rel>`.
pub fn desk(rel: &str) -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("BitsOS").join(rel)
}
