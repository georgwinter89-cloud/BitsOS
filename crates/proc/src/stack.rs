//! The initial stack of a new program, byte for byte as Linux builds it (pure, host-tested).
//!
//! Linux (fs/exec.c `copy_strings`, fs/binfmt_elf.c `create_elf_tables`), from the top down:
//! 8 zero bytes at the very top; the file name (AT_EXECFN); the environment strings and the argument strings,
//! so that `argv[0]` is the lowest string and the strings lie in order argv..., envp..., execfn; the top of what
//! follows aligned down to 16; the platform string "x86_64" (AT_PLATFORM); 16 random bytes (AT_RANDOM); then the
//! table `argc, argv[0..argc], NULL, envp[..], NULL, auxv pairs, AT_NULL` with its start (the program's rsp)
//! aligned down to 16. Linux also moves the top by a random amount (`arch_align_stack`, stack randomisation);
//! V2 does not randomise yet (open point in the notes).

use alloc::vec::Vec;
use uapi::ab::exec::MAX_ARG_STRLEN;

/// A value of the auxiliary vector: a number, or the address of something placed on the stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuxVal {
    Value(u64),
    ExecFn,
    Platform,
    Random,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    /// The program's rsp at entry (points at argc).
    pub rsp: u64,
    /// The bytes of `[rsp, top)`.
    pub bytes: Vec<u8>,
    /// Where things went (for tests and /proc later).
    pub arg_start: u64,
    pub env_start: u64,
    pub execfn: u64,
    pub platform: u64,
    pub random: u64,
    /// AB-5: the auxiliary vector as written, AT_NULL included (Linux `mm->saved_auxv`, PR_GET_AUXV).
    pub auxv: Vec<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackError {
    /// E2BIG: a string longer than MAX_ARG_STRLEN, or all of them above a quarter of the stack limit.
    TooBig,
    NoMemory,
}

pub const PLATFORM: &[u8] = b"x86_64";

/// Linux's argument space check (`bprm_stack_limits`) for a stack of `stack` bytes (the RLIMIT_STACK it was
/// made for gives the same limit): pointers and strings within a quarter of it, never below ARG_MAX nor above
/// 3/4 of `_STK_LIM`, each string at most `MAX_ARG_STRLEN` with its NUL.
pub fn check_size(stack: u64, argv: &[&[u8]], envp: &[&[u8]], execfn: &[u8]) -> Result<(), StackError> {
    let limit = uapi::ab::exec::arg_limit(stack);
    let ptrs = (argv.len().max(1) + envp.len()) as u64 * 8;
    if ptrs >= limit {
        return Err(StackError::TooBig);
    }
    let mut total = 0u64;
    for s in argv.iter().chain(envp.iter()).chain(core::iter::once(&execfn)) {
        if s.len() + 1 > MAX_ARG_STRLEN {
            return Err(StackError::TooBig);
        }
        total += s.len() as u64 + 1;
    }
    if total > limit - ptrs { Err(StackError::TooBig) } else { Ok(()) }
}

/// Builds the image for a stack whose highest address (exclusive) is `top`. `aux` lists the auxv pairs
/// without AT_NULL, in the order they should appear.
pub fn build(top: u64, stack: u64, argv: &[&[u8]], envp: &[&[u8]], execfn: &[u8], random: [u8; 16], aux: &[(u64, AuxVal)]) -> Result<Image, StackError> {
    check_size(stack, argv, envp, execfn)?;
    // Strings: (address, bytes) placed downward from top - 8.
    let mut p = top - 8;
    let place = |s: &[u8], p: &mut u64| -> u64 {
        *p -= s.len() as u64 + 1;
        *p
    };
    let execfn_at = place(execfn, &mut p);
    let mut env_at: Vec<u64> = Vec::new();
    let mut arg_at: Vec<u64> = Vec::new();
    env_at.try_reserve(envp.len()).map_err(|_| StackError::NoMemory)?;
    arg_at.try_reserve(argv.len()).map_err(|_| StackError::NoMemory)?;
    for s in envp.iter().rev() {
        env_at.push(place(s, &mut p));
    }
    env_at.reverse();
    for s in argv.iter().rev() {
        arg_at.push(place(s, &mut p));
    }
    arg_at.reverse();
    let arg_start = p;
    let env_start = env_at.first().copied().unwrap_or(execfn_at);
    p &= !15;
    p -= PLATFORM.len() as u64 + 1;
    let platform_at = p;
    p -= 16;
    let random_at = p;
    let aux_words = (aux.len() + 1) * 2;
    let items = (argv.len() + 1) + (envp.len() + 1) + 1;
    let rsp = (p - (aux_words + items) as u64 * 8) & !15;

    let len = (top - rsp) as usize;
    let mut b: Vec<u8> = Vec::new();
    b.try_reserve_exact(len).map_err(|_| StackError::NoMemory)?;
    b.resize(len, 0);
    let at = |a: u64| (a - rsp) as usize;
    let mut put = |a: u64, s: &[u8]| b[at(a)..at(a) + s.len()].copy_from_slice(s);
    put(execfn_at, execfn);
    for (s, a) in envp.iter().zip(&env_at) {
        put(*a, s);
    }
    for (s, a) in argv.iter().zip(&arg_at) {
        put(*a, s);
    }
    put(platform_at, PLATFORM);
    put(random_at, &random);
    let mut w = rsp;
    let mut word = |v: u64, w: &mut u64| {
        put(*w, &v.to_le_bytes());
        *w += 8;
    };
    word(argv.len() as u64, &mut w);
    for a in &arg_at {
        word(*a, &mut w);
    }
    word(0, &mut w);
    for a in &env_at {
        word(*a, &mut w);
    }
    word(0, &mut w);
    let mut saved = Vec::new();
    saved.try_reserve_exact(aux.len() * 2 + 2).map_err(|_| StackError::NoMemory)?;
    for (k, v) in aux {
        let val = match v {
            AuxVal::Value(x) => *x,
            AuxVal::ExecFn => execfn_at,
            AuxVal::Platform => platform_at,
            AuxVal::Random => random_at,
        };
        word(*k, &mut w);
        word(val, &mut w);
        saved.push(*k);
        saved.push(val);
    }
    word(uapi::auxv::AT_NULL, &mut w);
    word(0, &mut w);
    saved.push(uapi::auxv::AT_NULL);
    saved.push(0);
    Ok(Image { rsp, bytes: b, arg_start, env_start, execfn: execfn_at, platform: platform_at, random: random_at, auxv: saved })
}

/// Reads an initial stack back (the ktests check what a real program got with it): argc, the argv and envp
/// strings, the auxv pairs (up to AT_NULL). `mem(addr, len)` reads memory; `None` = unreadable.
pub struct Parsed {
    pub argv: Vec<Vec<u8>>,
    pub envp: Vec<Vec<u8>>,
    pub auxv: Vec<(u64, u64)>,
}

pub fn parse(rsp: u64, mem: &mut dyn FnMut(u64, usize) -> Option<Vec<u8>>, max_items: usize) -> Result<Parsed, &'static str> {
    let word = |a: u64, mem: &mut dyn FnMut(u64, usize) -> Option<Vec<u8>>| -> Result<u64, &'static str> {
        let b = mem(a, 8).ok_or("unreadable stack word")?;
        Ok(u64::from_le_bytes(b[..8].try_into().map_err(|_| "short read")?))
    };
    let cstr = |a: u64, mem: &mut dyn FnMut(u64, usize) -> Option<Vec<u8>>| -> Result<Vec<u8>, &'static str> {
        let mut out = Vec::new();
        for i in 0..MAX_ARG_STRLEN as u64 {
            let b = mem(a + i, 1).ok_or("unreadable string")?;
            if b[0] == 0 {
                return Ok(out);
            }
            out.push(b[0]);
        }
        Err("string without NUL")
    };
    let argc = word(rsp, mem)? as usize;
    if argc > max_items {
        return Err("argc implausible");
    }
    let mut a = rsp + 8;
    let mut argv = Vec::new();
    for _ in 0..argc {
        let p = word(a, mem)?;
        argv.push(cstr(p, mem)?);
        a += 8;
    }
    if word(a, mem)? != 0 {
        return Err("argv not NULL-terminated");
    }
    a += 8;
    let mut envp = Vec::new();
    loop {
        let p = word(a, mem)?;
        a += 8;
        if p == 0 {
            break;
        }
        if envp.len() >= max_items {
            return Err("envp not NULL-terminated");
        }
        envp.push(cstr(p, mem)?);
    }
    let mut auxv = Vec::new();
    loop {
        let k = word(a, mem)?;
        let v = word(a + 8, mem)?;
        a += 16;
        if k == uapi::auxv::AT_NULL {
            break;
        }
        if auxv.len() >= max_items {
            return Err("auxv without AT_NULL");
        }
        auxv.push((k, v));
    }
    Ok(Parsed { argv, envp, auxv })
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use uapi::auxv::*;

    const TOP: u64 = 0x0000_7FFF_FFFF_F000;

    fn img(argv: &[&[u8]], envp: &[&[u8]]) -> Image {
        let aux = [(AT_PAGESZ, AuxVal::Value(4096)), (AT_RANDOM, AuxVal::Random), (AT_EXECFN, AuxVal::ExecFn), (AT_PLATFORM, AuxVal::Platform)];
        build(TOP, 8 << 20, argv, envp, b"/bin/hello", [7; 16], &aux).unwrap()
    }

    fn reader(i: &Image) -> impl FnMut(u64, usize) -> Option<Vec<u8>> + '_ {
        move |a, n| {
            let o = a.checked_sub(i.rsp)? as usize;
            i.bytes.get(o..o + n).map(|s| s.to_vec())
        }
    }

    #[test]
    fn linux_layout() {
        let i = img(&[b"hello", b"-x"], &[b"HOME=/", b"PATH=/bin"]);
        assert_eq!(i.rsp % 16, 0, "rsp 16-byte aligned at entry");
        // Strings: argv lowest, then envp, then execfn, then 8 zero bytes at the top.
        assert_eq!(i.execfn, TOP - 8 - 11);
        assert_eq!(i.env_start, i.execfn - 10 - 7);
        assert_eq!(i.arg_start, i.env_start - 3 - 6);
        assert_eq!(&i.bytes[i.bytes.len() - 8..], &[0u8; 8]);
        // Platform right below the 16-aligned string area, random right below that (Linux: odd addresses).
        assert_eq!(i.platform, (i.arg_start & !15) - 7);
        assert_eq!(i.random, i.platform - 16);
        let p = parse(i.rsp, &mut reader(&i), 64).unwrap();
        assert_eq!(p.argv, [b"hello".to_vec(), b"-x".to_vec()]);
        assert_eq!(p.envp, [b"HOME=/".to_vec(), b"PATH=/bin".to_vec()]);
        assert_eq!(p.auxv, [(AT_PAGESZ, 4096), (AT_RANDOM, i.random), (AT_EXECFN, i.execfn), (AT_PLATFORM, i.platform)]);
        let at = |a: u64, n: usize| i.bytes[(a - i.rsp) as usize..][..n].to_vec();
        assert_eq!(at(i.random, 16), [7u8; 16]);
        assert_eq!(at(i.platform, 7), b"x86_64\0");
        assert_eq!(at(i.execfn, 11), b"/bin/hello\0");
        // The table ends below the random bytes.
        let table_end = i.rsp + 8 * (1 + 3 + 3 + 2 * 5);
        assert!(table_end <= i.random);
    }

    #[test]
    fn alignment_for_every_count() {
        for argc in 0..6usize {
            for envc in 0..5usize {
                for extra in 0..3usize {
                    let a: std::vec::Vec<std::vec::Vec<u8>> = (0..argc).map(|k| std::vec![b'a'; k + extra]).collect();
                    let e: std::vec::Vec<std::vec::Vec<u8>> = (0..envc).map(|k| std::vec![b'e'; k * 3 + 1]).collect();
                    let ar: std::vec::Vec<&[u8]> = a.iter().map(|v| v.as_slice()).collect();
                    let er: std::vec::Vec<&[u8]> = e.iter().map(|v| v.as_slice()).collect();
                    let i = img(&ar, &er);
                    assert_eq!(i.rsp % 16, 0);
                    let p = parse(i.rsp, &mut reader(&i), 64).unwrap();
                    assert_eq!(p.argv, a);
                    assert_eq!(p.envp, e);
                }
            }
        }
    }

    #[test]
    fn e2big_like_linux() {
        let long = std::vec![b'x'; MAX_ARG_STRLEN];
        assert_eq!(check_size(8 << 20, &[&long], &[], b"/x"), Err(StackError::TooBig), "string + NUL above MAX_ARG_STRLEN");
        let ok = std::vec![b'x'; MAX_ARG_STRLEN - 1];
        assert_eq!(check_size(8 << 20, &[&ok], &[], b"/x"), Ok(()));
        let many: std::vec::Vec<&[u8]> = (0..20).map(|_| ok.as_slice()).collect();
        assert_eq!(check_size(8 << 20, &many, &[], b"/x"), Err(StackError::TooBig), "20 x 128 KiB above 2 MiB");
        // AB-8: the limit follows RLIMIT_STACK (a quarter, at most 6 MiB, at least ARG_MAX).
        assert_eq!(check_size(64 << 20, &many, &[], b"/x"), Ok(()), "20 x 128 KiB under 6 MiB");
        let six: std::vec::Vec<&[u8]> = (0..48).map(|_| ok.as_slice()).collect();
        assert_eq!(check_size(1 << 30, &six, &[], b"/x"), Err(StackError::TooBig), "48 x 128 KiB above 3/4 of _STK_LIM");
        let small: std::vec::Vec<&[u8]> = (0..2).map(|_| &ok[..60 << 10]).collect();
        assert_eq!(check_size(256 << 10, &small, &[], b"/x"), Ok(()), "ARG_MAX stays");
        let ptrs: std::vec::Vec<&[u8]> = (0..(1 << 18)).map(|_| &b""[..]).collect();
        assert_eq!(check_size(8 << 20, &ptrs, &[], b"/x"), Err(StackError::TooBig), "pointers alone fill the space");
    }

    #[test]
    fn parse_refuses_garbage() {
        let i = img(&[b"a"], &[]);
        let mut short = |a: u64, n: usize| -> Option<Vec<u8>> { if a < i.rsp + 16 { reader(&i)(a, n) } else { None } };
        assert!(parse(i.rsp, &mut short, 64).is_err());
        let mut huge = |_a: u64, n: usize| -> Option<Vec<u8>> { Some(std::vec![0xFF; n]) };
        assert_eq!(parse(0, &mut huge, 64).err(), Some("argc implausible"));
    }
}
