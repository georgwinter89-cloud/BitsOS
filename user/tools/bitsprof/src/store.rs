//! `profile.bin`: the profile of one run, compact and self-contained (names included, so a report needs neither
//! the programs nor the kernel of that night). Little endian:
//!
//! ```text
//! "BITSPROF" u32 version(1) u32 0
//! strings: varint n, n × (varint len, bytes)
//! funcs:   varint n, n × (varint name, varint module)          (string indices; module "[kernel]" = kernel)
//! progs:   varint n, n × (varint name, varint pid)
//! stacks:  varint n, n × (varint depth, depth × varint func)   (innermost first)
//! windows: varint n, n × (varint start_ms, varint length_ms, varint idle, varint m, m × (varint prog, varint stack, varint samples))
//! ```
//! A window is a stretch of the run (30 s); when the file would pass its size cap the oldest windows go first.

use std::collections::HashMap;

pub const MAGIC: &[u8; 8] = b"BITSPROF";
pub const VERSION: u32 = 1;
/// The module name of kernel functions.
pub const KERNEL: &str = "[kernel]";

/// One stretch of the run: how often each (program, stack) was seen, and the idle samples.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Window {
    pub start_ms: u64,
    pub len_ms: u64,
    pub idle: u64,
    pub counts: Vec<(u32, u32, u64)>,
}

/// A whole profile as it is stored (and as `report` reads it).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Profile {
    pub strings: Vec<String>,
    /// (name, module) as string indices.
    pub funcs: Vec<(u32, u32)>,
    /// (name, pid).
    pub progs: Vec<(u32, u32)>,
    pub stacks: Vec<Vec<u32>>,
    pub windows: Vec<Window>,
}

/// Interning tables that build a `Profile` while the run goes on.
#[derive(Default)]
pub struct Builder {
    pub p: Profile,
    strings: HashMap<String, u32>,
    funcs: HashMap<(u32, u32), u32>,
    progs: HashMap<(u32, u32), u32>,
    stacks: HashMap<Vec<u32>, u32>,
}

impl Builder {
    pub fn string(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.strings.get(s) {
            return i;
        }
        let i = self.p.strings.len() as u32;
        self.p.strings.push(s.to_string());
        self.strings.insert(s.to_string(), i);
        i
    }
    pub fn func(&mut self, name: &str, module: &str) -> u32 {
        let k = (self.string(name), self.string(module));
        let n = self.p.funcs.len() as u32;
        *self.funcs.entry(k).or_insert_with(|| {
            self.p.funcs.push(k);
            n
        })
    }
    pub fn prog(&mut self, name: &str, pid: u32) -> u32 {
        let k = (self.string(name), pid);
        let n = self.p.progs.len() as u32;
        *self.progs.entry(k).or_insert_with(|| {
            self.p.progs.push(k);
            n
        })
    }
    pub fn stack(&mut self, frames: &[u32]) -> u32 {
        if let Some(&i) = self.stacks.get(frames) {
            return i;
        }
        let i = self.p.stacks.len() as u32;
        self.p.stacks.push(frames.to_vec());
        self.stacks.insert(frames.to_vec(), i);
        i
    }
}

fn put(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

struct Rd<'a> {
    b: &'a [u8],
    at: usize,
}

impl Rd<'_> {
    fn v(&mut self) -> Result<u64, String> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = *self.b.get(self.at).ok_or("profile.bin ends early")?;
            self.at += 1;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err("profile.bin: a number is too long".into())
    }
    fn n(&mut self, max: usize) -> Result<usize, String> {
        let v = self.v()? as usize;
        if v > max { Err("profile.bin: a count is larger than the file".into()) } else { Ok(v) }
    }
    fn u32(&mut self) -> Result<u32, String> {
        u32::try_from(self.v()?).map_err(|_| "profile.bin: an index is too large".to_string())
    }
}

impl Profile {
    /// The bytes of the profile with only windows `from..` (the cap drops the ones before), without the
    /// functions, programs, stacks and strings no kept window uses.
    pub fn encode(&self, from: usize) -> Vec<u8> {
        let wins = &self.windows[from.min(self.windows.len())..];
        let mut use_stack = vec![false; self.stacks.len()];
        let mut use_prog = vec![false; self.progs.len()];
        for w in wins {
            for &(p, s, _) in &w.counts {
                use_prog[p as usize] = true;
                use_stack[s as usize] = true;
            }
        }
        let mut use_func = vec![false; self.funcs.len()];
        for (i, s) in self.stacks.iter().enumerate() {
            if use_stack[i] {
                s.iter().for_each(|&f| use_func[f as usize] = true);
            }
        }
        let mut use_str = vec![false; self.strings.len()];
        for (i, &(n, m)) in self.funcs.iter().enumerate() {
            if use_func[i] {
                use_str[n as usize] = true;
                use_str[m as usize] = true;
            }
        }
        for (i, &(n, _)) in self.progs.iter().enumerate() {
            if use_prog[i] {
                use_str[n as usize] = true;
            }
        }
        let remap = |used: &[bool]| {
            let mut m = vec![u32::MAX; used.len()];
            let mut n = 0;
            for (i, &u) in used.iter().enumerate() {
                if u {
                    m[i] = n;
                    n += 1;
                }
            }
            (m, n)
        };
        let (rs, ns) = remap(&use_str);
        let (rf, nf) = remap(&use_func);
        let (rp, np) = remap(&use_prog);
        let (rk, nk) = remap(&use_stack);
        let mut o = Vec::new();
        o.extend_from_slice(MAGIC);
        o.extend_from_slice(&VERSION.to_le_bytes());
        o.extend_from_slice(&0u32.to_le_bytes());
        put(&mut o, ns as u64);
        for (i, s) in self.strings.iter().enumerate() {
            if use_str[i] {
                put(&mut o, s.len() as u64);
                o.extend_from_slice(s.as_bytes());
            }
        }
        put(&mut o, nf as u64);
        for (i, &(n, m)) in self.funcs.iter().enumerate() {
            if use_func[i] {
                put(&mut o, rs[n as usize] as u64);
                put(&mut o, rs[m as usize] as u64);
            }
        }
        put(&mut o, np as u64);
        for (i, &(n, pid)) in self.progs.iter().enumerate() {
            if use_prog[i] {
                put(&mut o, rs[n as usize] as u64);
                put(&mut o, pid as u64);
            }
        }
        put(&mut o, nk as u64);
        for (i, s) in self.stacks.iter().enumerate() {
            if use_stack[i] {
                put(&mut o, s.len() as u64);
                s.iter().for_each(|&f| put(&mut o, rf[f as usize] as u64));
            }
        }
        put(&mut o, wins.len() as u64);
        for w in wins {
            put(&mut o, w.start_ms);
            put(&mut o, w.len_ms);
            put(&mut o, w.idle);
            put(&mut o, w.counts.len() as u64);
            for &(p, s, c) in &w.counts {
                put(&mut o, rp[p as usize] as u64);
                put(&mut o, rk[s as usize] as u64);
                put(&mut o, c);
            }
        }
        o
    }

    pub fn decode(b: &[u8]) -> Result<Profile, String> {
        if b.len() < 16 || &b[..8] != MAGIC {
            return Err("profile.bin: not a bitsprof profile".into());
        }
        let ver = u32::from_le_bytes([b[8], b[9], b[10], b[11]]);
        if ver != VERSION {
            return Err(format!("profile.bin: version {ver}, this bitsprof reads {VERSION}"));
        }
        let mut r = Rd { b, at: 16 };
        let max = b.len();
        let mut p = Profile::default();
        for _ in 0..r.n(max)? {
            let len = r.n(max)?;
            let s = b.get(r.at..r.at + len).ok_or("profile.bin ends inside a name")?;
            r.at += len;
            p.strings.push(String::from_utf8_lossy(s).into_owned());
        }
        let ns = p.strings.len() as u32;
        let check = |i: u32, n: u32| if i < n { Ok(i) } else { Err("profile.bin: an index points past its table".to_string()) };
        for _ in 0..r.n(max)? {
            p.funcs.push((check(r.u32()?, ns)?, check(r.u32()?, ns)?));
        }
        for _ in 0..r.n(max)? {
            p.progs.push((check(r.u32()?, ns)?, r.u32()?));
        }
        let nf = p.funcs.len() as u32;
        for _ in 0..r.n(max)? {
            let d = r.n(max)?;
            let mut s = Vec::with_capacity(d);
            for _ in 0..d {
                s.push(check(r.u32()?, nf)?);
            }
            p.stacks.push(s);
        }
        let (np, nk) = (p.progs.len() as u32, p.stacks.len() as u32);
        for _ in 0..r.n(max)? {
            let mut w = Window { start_ms: r.v()?, len_ms: r.v()?, idle: r.v()?, counts: Vec::new() };
            for _ in 0..r.n(max)? {
                w.counts.push((check(r.u32()?, np)?, check(r.u32()?, nk)?, r.v()?));
            }
            p.windows.push(w);
        }
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_cap() {
        let mut b = Builder::default();
        let f1 = b.func("hot", "/bin/a");
        let f2 = b.func("sched::run", KERNEL);
        let f3 = b.func("old_only", "/bin/a");
        let pa = b.prog("a", 7);
        let s1 = b.stack(&[f1]);
        let s2 = b.stack(&[f2, f1]);
        let s3 = b.stack(&[f3]);
        assert_eq!(b.stack(&[f1]), s1);
        b.p.windows.push(Window { start_ms: 0, len_ms: 30_000, idle: 3, counts: vec![(pa, s3, 9)] });
        b.p.windows.push(Window { start_ms: 30_000, len_ms: 30_000, idle: 5, counts: vec![(pa, s1, 10), (pa, s2, 4)] });
        let all = Profile::decode(&b.p.encode(0)).unwrap();
        assert_eq!(all, b.p);
        // Dropping the first window drops the function only it used.
        let late = Profile::decode(&b.p.encode(1)).unwrap();
        assert_eq!(late.windows.len(), 1);
        assert_eq!(late.funcs.len(), 2);
        assert!(!late.strings.iter().any(|s| s == "old_only"));
        assert_eq!(late.windows[0].counts.iter().map(|c| c.2).sum::<u64>(), 14);
        assert!(Profile::decode(b"BITSPROF\x02\0\0\0\0\0\0\0").is_err());
        assert!(Profile::decode(&b.p.encode(0)[..20]).is_err());
    }
}
