//! C1: test knobs exist only in the build the tests make (`--features test-knobs`, which the integration
//! tests turn on themselves). The shipped binary reads no `CTXD_TEST_*` variable and has no fake embedder.

pub const ON: bool = cfg!(feature = "test-knobs");

/// A `CTXD_TEST_*` variable, read only in the test build.
pub fn var(name: &str) -> Option<String> {
    if !ON {
        return None;
    }
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

pub fn millis(name: &str) -> Option<u64> {
    var(name).and_then(|v| v.parse().ok())
}
