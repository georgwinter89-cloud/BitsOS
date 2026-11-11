//! The pure parts of blkbench: arguments, the generator, the small files' sizes, statistics and the output
//! lines. Host-tested (`cargo test -p blkbench`).

/// Clock ticks per second of /proc/<pid>/stat (USER_HZ, 100 on x86-64 Linux and on V2).
pub const USER_HZ: u64 = 100;

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Prepare { dir: String, seq_mb: u64, rand_mb: u64, small: u64, seed: u64 },
    Cold { paths: Vec<String> },
    Seq { file: String, bs: u64, threads: u64, runs: u64 },
    Rand { file: String, bs: u64, count: u64, threads: u64, runs: u64 },
    Small { dir: String, runs: u64 },
    Mmap { file: String, runs: u64 },
    All { dir: String, runs: u64 },
}

pub const USAGE: &str = "usage: blkbench prepare DIR [--seq-mb 4096] [--rand-mb 1024] [--small 4000] [--seed 35]
       blkbench cold FILE|DIR...
       blkbench seq FILE [--bs 1M] [--threads N] [--runs 3]
       blkbench rand FILE [--bs 4K] [--count 20000] [--threads 1|8|32] [--runs 3]
       blkbench small DIR [--runs 3]
       blkbench mmap FILE [--runs 3]
       blkbench all DIR [--runs 3]";

/// "4K", "1M", "1G" or a plain number of bytes.
pub fn size(s: &str) -> Option<u64> {
    let (num, mul) = match s.as_bytes().last()? {
        b'K' | b'k' => (&s[..s.len() - 1], 1 << 10),
        b'M' | b'm' => (&s[..s.len() - 1], 1 << 20),
        b'G' | b'g' => (&s[..s.len() - 1], 1 << 30),
        _ => (s, 1),
    };
    num.parse::<u64>().ok()?.checked_mul(mul).filter(|&v| v > 0)
}

pub fn parse(args: &[String]) -> Result<Cmd, String> {
    let (cmd, rest) = args.split_first().ok_or("no command")?;
    let mut pos = Vec::new();
    let mut opt = std::collections::BTreeMap::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if let Some(k) = a.strip_prefix("--") {
            let v = it.next().ok_or_else(|| format!("--{k} needs a value"))?;
            opt.insert(k.to_string(), v.clone());
        } else {
            pos.push(a.clone());
        }
    }
    let mut take = |k: &str, def: u64, sized: bool| -> Result<u64, String> {
        match opt.remove(k) {
            None => Ok(def),
            Some(v) => if sized { size(&v) } else { v.parse().ok().filter(|&n: &u64| n > 0) }.ok_or_else(|| format!("--{k} {v}: not a positive number")),
        }
    };
    let one = |pos: &[String]| -> Result<String, String> {
        match pos {
            [p] => Ok(p.clone()),
            _ => Err(format!("{cmd} takes one path, got {}", pos.len())),
        }
    };
    let c = match cmd.as_str() {
        "prepare" => Cmd::Prepare { dir: one(&pos)?, seq_mb: take("seq-mb", 4096, false)?, rand_mb: take("rand-mb", 1024, false)?, small: take("small", 4000, false)?, seed: take("seed", 35, false)? },
        "cold" if !pos.is_empty() => Cmd::Cold { paths: pos.clone() },
        "cold" => return Err("cold takes at least one path".into()),
        "seq" => Cmd::Seq { file: one(&pos)?, bs: take("bs", 1 << 20, true)?, threads: take("threads", 1, false)?, runs: take("runs", 3, false)? },
        "rand" => Cmd::Rand { file: one(&pos)?, bs: take("bs", 4096, true)?, count: take("count", 20000, false)?, threads: take("threads", 1, false)?, runs: take("runs", 3, false)? },
        "small" => Cmd::Small { dir: one(&pos)?, runs: take("runs", 3, false)? },
        "mmap" => Cmd::Mmap { file: one(&pos)?, runs: take("runs", 3, false)? },
        "all" => Cmd::All { dir: one(&pos)?, runs: take("runs", 3, false)? },
        _ => return Err(format!("unknown command {cmd}")),
    };
    if let Some(k) = opt.keys().next() {
        return Err(format!("{cmd}: unknown option --{k}"));
    }
    Ok(c)
}

/// xorshift64* (deterministic content and offsets on both systems).
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// A float in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn fill(&mut self, buf: &mut [u8]) {
        for c in buf.chunks_mut(8) {
            let v = self.next().to_le_bytes();
            c.copy_from_slice(&v[..c.len()]);
        }
    }
}

/// The small files' sizes: log-uniform from 4 KiB to 1 MiB.
pub fn small_sizes(n: u64, seed: u64) -> Vec<u64> {
    let mut r = Rng::new(seed);
    let (lo, hi) = ((4096f64).ln(), ((1u64 << 20) as f64).ln());
    (0..n).map(|_| ((lo + r.unit() * (hi - lo)).exp() as u64).clamp(4096, 1 << 20)).collect()
}

pub fn small_name(i: u64) -> String {
    format!("f{i:05}")
}

/// The median (the lower middle of an even count); 0 for none.
pub fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    if s.is_empty() { 0.0 } else { s[(s.len() - 1) / 2] }
}

/// The p-th percentile (nearest rank) of `v`, sorted here.
pub fn percentile(v: &mut [u64], p: f64) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    let rank = ((p / 100.0) * v.len() as f64).ceil() as usize;
    v[rank.clamp(1, v.len()) - 1]
}

/// (utime, stime) ticks from /proc/self/stat's text: fields 14 and 15, counted after the comm's last ')'.
pub fn stat_ticks(s: &str) -> Option<(u64, u64)> {
    let rest = &s[s.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    // After ')' the first field is 3 (state): utime is field 14 -> index 11.
    Some((f.get(11)?.parse().ok()?, f.get(12)?.parse().ok()?))
}

/// One output line: `test=<name> k=v ...`, floats with one decimal.
pub fn line(test: &str, kv: &[(&str, String)]) -> String {
    let mut s = format!("test={test}");
    for (k, v) in kv {
        s.push(' ');
        s.push_str(k);
        s.push('=');
        s.push_str(v);
    }
    s
}

pub fn f1(v: f64) -> String {
    format!("{v:.1}")
}

pub fn list(v: &[f64]) -> String {
    v.iter().map(|x| f1(*x)).collect::<Vec<_>>().join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn commands_parse_with_defaults_and_options() {
        assert_eq!(parse(&a("seq /d/seq.bin")), Ok(Cmd::Seq { file: "/d/seq.bin".into(), bs: 1 << 20, threads: 1, runs: 3 }));
        assert_eq!(parse(&a("rand f --bs 4K --count 100 --threads 32")), Ok(Cmd::Rand { file: "f".into(), bs: 4096, count: 100, threads: 32, runs: 3 }));
        assert_eq!(parse(&a("prepare /data/bench --seq-mb 64")), Ok(Cmd::Prepare { dir: "/data/bench".into(), seq_mb: 64, rand_mb: 1024, small: 4000, seed: 35 }));
        assert_eq!(parse(&a("cold a b")), Ok(Cmd::Cold { paths: vec!["a".into(), "b".into()] }));
        assert_eq!(parse(&a("all /x --runs 1")), Ok(Cmd::All { dir: "/x".into(), runs: 1 }));
    }

    #[test]
    fn bad_arguments_name_themselves() {
        assert!(parse(&a("seq")).unwrap_err().contains("one path"));
        assert!(parse(&a("seq f --bs 0")).unwrap_err().contains("--bs"));
        assert!(parse(&a("seq f --bs")).unwrap_err().contains("needs a value"));
        assert!(parse(&a("rand f --depth 3")).unwrap_err().contains("--depth"));
        assert!(parse(&a("cold")).is_err() && parse(&a("frob x")).is_err() && parse(&[]).is_err());
        assert_eq!(size("1M"), Some(1 << 20));
        assert_eq!(size("4k"), Some(4096));
        assert_eq!((size("x"), size("99999999999999999999G"), size("")), (None, None, None));
    }

    #[test]
    fn statistics() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), 2.0);
        assert_eq!(median(&[]), 0.0);
        let mut v: Vec<u64> = (1..=100).collect();
        assert_eq!((percentile(&mut v, 50.0), percentile(&mut v, 99.0), percentile(&mut v, 100.0)), (50, 99, 100));
        assert_eq!(percentile(&mut [7], 99.0), 7);
        assert_eq!(percentile(&mut [], 50.0), 0);
    }

    #[test]
    fn small_files_are_log_uniform_and_seeded() {
        let s = small_sizes(4000, 35);
        assert_eq!(s, small_sizes(4000, 35), "the same seed gives the same set on both systems");
        assert!(s.iter().all(|&x| (4096..=1 << 20).contains(&x)));
        let below_64k = s.iter().filter(|&&x| x < 65536).count();
        // log-uniform: 4K..64K is half of the log range.
        assert!((1800..2200).contains(&below_64k), "{below_64k}");
        assert_eq!(small_name(7), "f00007");
    }

    #[test]
    fn stat_fields_and_lines() {
        let st = "1234 (blk bench) R 1 1234 1234 0 -1 4194560 100 0 0 0 57 13 0 0 20 0 1 0 100 1000 10";
        assert_eq!(stat_ticks(st), Some((57, 13)));
        assert_eq!(stat_ticks("garbage"), None);
        assert_eq!(line("seq", &[("mb_s", f1(12.34)), ("runs", list(&[1.0, 2.25]))]), "test=seq mb_s=12.3 runs=1.0,2.2");
        let mut r = Rng::new(35);
        let mut b = [0u8; 13];
        r.fill(&mut b);
        assert!(b.iter().any(|&x| x != 0));
    }
}
