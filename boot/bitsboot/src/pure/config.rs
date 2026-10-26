//! The few CONFIG.TXT keys bitsboot itself reads. Same rules as the kernel's parser (`spec/core.md` §2.6):
//! `name=value`, `#` starts a comment, spaces around name and value ignored, the last line of a name wins.
//! The whole file goes to the kernel unchanged (CONFIG record); this only looks things up.

/// The value of `key`, or `None` when no line sets it.
pub fn get<'a>(text: &'a [u8], key: &str) -> Option<&'a str> {
    let mut found = None;
    for raw in text.split(|&b| b == b'\n') {
        // A line that is not UTF-8 cannot hold one of bitsboot's keys; skip it instead of failing.
        let Ok(line) = core::str::from_utf8(raw) else { continue };
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            found = Some(v.trim());
        }
    }
    found
}

/// Test hooks for the ExitBootServices retry. Honoured only under a hypervisor (QEMU), so a stray line on the
/// real stick changes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestHook {
    None,
    /// Make the first ExitBootServices fail with a stale map key (the retry must succeed).
    EbsRetry,
    /// Make every ExitBootServices fail (the readable error and the return to the firmware must work).
    EbsFail,
    /// BT-edid: the EDID Active protocol answers with `edid::fixture()` (QEMU's OVMF installs no EDID protocol).
    EdidActive,
    /// BT-edid: Active answers with a broken header, Discovered with `edid::fixture()` (the fallback path).
    EdidFallback,
}

pub const TEST_KEY: &str = "bitsboot.test";

pub fn test_hook(text: &[u8]) -> TestHook {
    match get(text, TEST_KEY) {
        Some("ebs_retry") => TestHook::EbsRetry,
        Some("ebs_fail") => TestHook::EbsFail,
        Some("edid_active") => TestHook::EdidActive,
        Some("edid_fallback") => TestHook::EdidFallback,
        _ => TestHook::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_line_wins_comments_and_spaces() {
        let t = b"# screen=1x1\nscreen = 1024x768 \nfoo=1\n  screen=2560x1440 # native\n";
        assert_eq!(get(t, "screen"), Some("2560x1440"));
        assert_eq!(get(t, "foo"), Some("1"));
        assert_eq!(get(t, "bar"), None);
    }

    #[test]
    fn crlf_and_bad_utf8_lines() {
        let t = b"a=1\r\n\xff\xfe=2\nb = x\r\n";
        assert_eq!(get(t, "a"), Some("1"));
        assert_eq!(get(t, "b"), Some("x"));
    }

    #[test]
    fn hooks() {
        assert_eq!(test_hook(b"bitsboot.test=ebs_retry\n"), TestHook::EbsRetry);
        assert_eq!(test_hook(b"bitsboot.test=ebs_fail"), TestHook::EbsFail);
        assert_eq!(test_hook(b"bitsboot.test=edid_active"), TestHook::EdidActive);
        assert_eq!(test_hook(b"bitsboot.test=edid_fallback"), TestHook::EdidFallback);
        assert_eq!(test_hook(b"bitsboot.test=other"), TestHook::None);
        assert_eq!(test_hook(b""), TestHook::None);
    }

    #[test]
    fn huge_text() {
        let mut t = std::vec::Vec::new();
        for _ in 0..200_000 {
            t.extend_from_slice(b"# padding line to make the file large\n");
        }
        t.extend_from_slice(b"bitsboot.test=ebs_retry\n");
        assert_eq!(test_hook(&t), TestHook::EbsRetry);
    }
}
