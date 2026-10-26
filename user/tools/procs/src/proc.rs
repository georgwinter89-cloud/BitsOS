//! Reading one process from /proc/<pid>/{stat,status,cmdline,comm}; every value optional.

use std::collections::BTreeMap;

pub struct Proc {
    pub pid: u32,
    pub comm: String,
    pub state: char,
    pub ppid: Option<u32>,
    pub uid: Option<u32>,
    pub threads: Option<u32>,
    pub rss_kb: Option<u64>,
    pub vsz_kb: Option<u64>,
    /// utime + stime in USER_HZ ticks.
    pub ticks: Option<u64>,
    pub argv: Vec<String>,
}

impl Proc {
    /// The command line, or `[comm]` when it is empty (kernel threads, zombies), as ps shows them.
    pub fn cmd(&self) -> String {
        if self.argv.is_empty() { format!("[{}]", self.comm) } else { self.argv.join(" ") }
    }
}

/// The numeric entries of /proc, sorted.
pub fn list_pids() -> std::io::Result<Vec<u32>> {
    let mut v: Vec<u32> = std::fs::read_dir("/proc")?.flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect();
    v.sort_unstable();
    Ok(v)
}

/// `pid (comm) S f4 f5 ...`: comm and the fields after it (index 0 = field 3, the state).
pub fn parse_stat(s: &str) -> Option<(String, Vec<&str>)> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    if close < open {
        return None;
    }
    Some((s[open + 1..close].to_string(), s[close + 1..].split_whitespace().collect()))
}

/// `Key:\tvalue` lines of /proc/<pid>/status or /proc/meminfo; the first number of each value.
pub fn key_numbers(s: &str) -> BTreeMap<String, u64> {
    s.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.trim().to_string(), v.split_whitespace().next()?.parse().ok()?))
        })
        .collect()
}

pub fn meminfo() -> BTreeMap<String, u64> {
    std::fs::read_to_string("/proc/meminfo").map(|s| key_numbers(&s)).unwrap_or_default()
}

/// NUL-separated arguments; an empty argument stays (`sh -c ''`), only the piece after the final NUL goes.
pub fn split_cmdline(b: &[u8]) -> Vec<String> {
    if b.is_empty() {
        return Vec::new();
    }
    let b = b.strip_suffix(&[0]).unwrap_or(b);
    b.split(|&c| c == 0).map(|a| String::from_utf8_lossy(a).into_owned()).collect()
}

/// None when the process is gone (or its stat unreadable): the caller skips it.
pub fn read_proc(pid: u32) -> Option<Proc> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (comm, f) = parse_stat(&stat)?;
    // Field n of do_task_stat is f[n - 3].
    let field = |n: usize| f.get(n - 3).and_then(|v| v.parse::<u64>().ok());
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).map(|s| key_numbers(&s)).unwrap_or_default();
    let argv = std::fs::read(format!("/proc/{pid}/cmdline")).map(|b| split_cmdline(&b)).unwrap_or_default();
    Some(Proc {
        pid,
        comm,
        state: f.first().and_then(|s| s.chars().next()).unwrap_or('?'),
        ppid: field(4).map(|v| v as u32),
        uid: status.get("Uid").map(|v| *v as u32),
        threads: field(20).or_else(|| status.get("Threads").copied()).map(|v| v as u32),
        rss_kb: field(24).map(|p| p * 4).or_else(|| status.get("VmRSS").copied()),
        vsz_kb: field(23).map(|b| b / 1024).or_else(|| status.get("VmSize").copied()),
        ticks: field(14).zip(field(15)).map(|(u, s)| u + s),
        argv,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_with_odd_comm() {
        let (c, f) = parse_stat("42 (a) b (c)) S 1 42 42 0 -1").unwrap();
        assert_eq!(c, "a) b (c)");
        assert_eq!(f[0], "S");
        assert_eq!(f[1], "1");
        assert!(parse_stat("garbage").is_none());
    }

    #[test]
    fn cmdline_keeps_empty_arguments() {
        assert_eq!(split_cmdline(b"sh\0-c\0\0"), ["sh", "-c", ""]);
        assert_eq!(split_cmdline(b"a\0b\0"), ["a", "b"]);
        assert_eq!(split_cmdline(b"no-nul"), ["no-nul"]);
        assert!(split_cmdline(b"").is_empty());
    }

    #[test]
    fn status_numbers() {
        let m = key_numbers("Name:\tx\nUid:\t1000\t1000\t1000\t1000\nVmRSS:\t   123 kB\n");
        assert_eq!(m.get("Uid"), Some(&1000));
        assert_eq!(m.get("VmRSS"), Some(&123));
        assert!(!m.contains_key("Name"));
    }
}
