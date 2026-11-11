//! Which release this installer installs and where it comes from. The version and the manifest's SHA-256 are fixed
//! when the installer is built (`cargo xtask release-assets` builds it with both); a build without them installs
//! only from `--assets DIR` with the hash given by hand.

/// The one place the project's address is written down.
pub const REPOSITORY: &str = "https://github.com/georgwinter89-cloud/BitsOS";

pub const VERSION: &str = match option_env!("BITSOS_RELEASE_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// The release manifest's SHA-256 (64 hex digits) this build trusts; `build.rs` checks its form.
pub const EMBEDDED_MANIFEST_SHA256: Option<&str> = option_env!("BITSOS_RELEASE_MANIFEST_SHA256");

pub fn asset_url(name: &str) -> String {
    format!("{REPOSITORY}/releases/download/v{VERSION}/{name}")
}

pub fn asset_name(what: &str, version: &str) -> String {
    format!("bitsos-{what}-{version}.tar.zst")
}
