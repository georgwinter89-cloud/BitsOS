//! System generations `\BITSOS2\SYS\<n>\` on a stick that already holds BitsOS, with the rules of `cargo xtask
//! stick` (host/xtask/src/stick/sys.rs) and bitsboot: the next number, an unconfirmed newest one replaced, and the
//! pruning that keeps the newest, the pinned and the newest `sys_keep` confirmed generations.

use crate::fat;
use fatfs::{Dir, ReadWriteSeek};

pub const SYS_KEEP_DEFAULT: u32 = 5;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Gen {
    pub number: u32,
    pub name: String,
    pub kernel: bool,
    pub initrd: bool,
    pub ok: bool,
    pub new: bool,
    pub tried: bool,
}

impl Gen {
    pub fn markers(&self) -> String {
        let on: Vec<&str> = [(self.new, "NEW"), (self.tried, "TRY"), (self.ok, "OK")].iter().filter(|(b, _)| *b).map(|(_, n)| *n).collect();
        if on.is_empty() { "no marker".into() } else { on.join("+") }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sys {
    pub gens: Vec<Gen>,
    pub pin: Option<u32>,
    pub staging: bool,
}

impl Sys {
    pub fn summary(&self) -> String {
        if self.gens.is_empty() {
            return "no generation".into();
        }
        self.gens.iter().map(|g| format!("{} {}", g.number, g.markers())).collect::<Vec<_>>().join(", ")
    }

    /// The number the new generation gets, and the never-confirmed newest one it takes the place of.
    pub fn next(&self) -> Result<(u32, Option<Gen>), String> {
        match self.gens.last() {
            None => Ok((1, None)),
            Some(g) if !g.ok => Ok((g.number, Some(g.clone()))),
            Some(g) => g.number.checked_add(1).map(|n| (n, None)).ok_or_else(|| "the newest generation number is already the largest".to_string()),
        }
    }
}

pub fn parse_name(name: &str) -> Option<u32> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    name.parse().ok()
}

pub fn scan<T: ReadWriteSeek>(root: &Dir<T>) -> Result<Sys, String> {
    let mut st = Sys::default();
    let Ok(sys) = root.open_dir("BITSOS2/SYS") else { return Ok(st) };
    for e in sys.iter() {
        let e = e.map_err(|e| format!("\\BITSOS2\\SYS: {e}"))?;
        let name = e.file_name();
        if name == "." || name == ".." {
            continue;
        }
        if e.is_dir() && let Some(number) = parse_name(&name) {
            if st.gens.iter().any(|g| g.number == number) {
                return Err(format!("\\BITSOS2\\SYS has two folders for generation {number}; sort that out by hand first"));
            }
            let mut g = Gen { number, name: name.clone(), ..Gen::default() };
            for f in e.to_dir().iter().flatten().filter(|f| f.is_file()) {
                let n = f.file_name().to_ascii_uppercase();
                g.kernel |= n == "KERNEL";
                g.initrd |= n == "INITRD";
                g.new |= n == "NEW";
                g.tried |= n == "TRY";
                g.ok |= n == "OK";
            }
            st.gens.push(g);
        } else if e.is_dir() && name.eq_ignore_ascii_case("STAGING") {
            st.staging = true;
        } else if e.is_file() && name.eq_ignore_ascii_case("PIN") {
            let t = fat::read_small(root, "BITSOS2/SYS/PIN", 4096).unwrap_or_default();
            st.pin = std::str::from_utf8(&t).ok().and_then(|t| parse_name(t.trim()));
        }
    }
    st.gens.sort_by_key(|g| g.number);
    Ok(st)
}

/// `sys_keep=` of a CONFIG text (the last line wins); None when it is there but no number (nothing is pruned then).
pub fn sys_keep(config: Option<&[u8]>) -> Option<u32> {
    let Some(text) = config else { return Some(SYS_KEEP_DEFAULT) };
    let mut found: Option<String> = None;
    for l in String::from_utf8_lossy(text).lines() {
        let l = l.split('#').next().unwrap_or("").trim();
        if let Some((k, v)) = l.split_once('=')
            && k.trim() == "sys_keep"
        {
            found = Some(v.trim().to_string());
        }
    }
    match found {
        None => Some(SYS_KEEP_DEFAULT),
        Some(v) => v.parse::<u32>().ok().map(|n| n.max(1)),
    }
}

/// The generations to delete (xtask stick's rule): all but the newest, the newest `keep` confirmed ones and the pin.
pub fn prune(gens: &[Gen], keep: u32, pin: Option<u32>) -> Vec<u32> {
    let keep = keep.max(1) as usize;
    let newest = gens.iter().map(|g| g.number).max();
    let mut oks: Vec<u32> = gens.iter().filter(|g| g.ok).map(|g| g.number).collect();
    oks.sort_unstable_by(|a, b| b.cmp(a));
    let kept: Vec<u32> = oks.into_iter().take(keep).chain(newest).chain(pin).collect();
    gens.iter().map(|g| g.number).filter(|n| !kept.contains(n)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(n: u32, ok: bool) -> Gen {
        Gen { number: n, name: n.to_string(), ok, new: !ok, kernel: true, initrd: true, ..Gen::default() }
    }

    #[test]
    fn numbering_and_pruning_follow_xtask_stick() {
        let st = Sys { gens: vec![g(1, true), g(2, true)], ..Sys::default() };
        assert_eq!(st.next().unwrap(), (3, None));
        let st = Sys { gens: vec![g(1, true), g(2, false)], ..Sys::default() };
        assert_eq!(st.next().unwrap().0, 2, "an unconfirmed newest one is replaced");
        assert_eq!(Sys::default().next().unwrap(), (1, None));
        let gens: Vec<Gen> = (1..=7).map(|n| g(n, n != 7)).collect();
        assert_eq!(prune(&gens, 2, None), vec![1, 2, 3, 4]);
        assert_eq!(prune(&gens, 2, Some(1)), vec![2, 3, 4]);
        assert_eq!(sys_keep(Some(b"sys_keep=3\n# sys_keep=9\n")), Some(3));
        assert_eq!(sys_keep(Some(b"sys_keep=x")), None);
        assert_eq!(sys_keep(None), Some(5));
    }
}
