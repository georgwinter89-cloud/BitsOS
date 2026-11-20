//! How much one answer may hold in memory. Not a constant (R1): a share of the memory that is free when the
//! request starts, so a provider or a tool that sends without end is stopped by what this machine can hold.

/// A quarter of what the kernel calls available: the answer exists as bytes, as text and inside the next request.
const SHARE: u64 = 4;

/// Bytes one answer (or one tool result) may take.
pub fn budget() -> usize {
    budget_from(std::fs::read_to_string("/proc/meminfo").ok().as_deref())
}

pub fn budget_from(meminfo: Option<&str>) -> usize {
    let kb = meminfo.and_then(|m| {
        m.lines().find_map(|l| l.strip_prefix("MemAvailable:")?.trim().strip_suffix("kB")?.trim().parse::<u64>().ok())
    });
    match kb {
        Some(kb) => usize::try_from(kb.saturating_mul(1024) / SHARE).unwrap_or(usize::MAX),
        // No /proc/meminfo: the address space is the only bound left to name.
        None => usize::MAX / 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_share_of_what_is_available() {
        assert_eq!(budget_from(Some("MemTotal: 100 kB\nMemAvailable:    4096 kB\n")), 1024 * 1024);
        assert_eq!(budget_from(None), usize::MAX / 2);
    }
}
