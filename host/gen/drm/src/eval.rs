//! Integer constant expressions with C's types on x86_64 (int, unsigned int, long, unsigned long), macro
//! expansion, and the ioctl number macros of asm-generic/ioctl.h (_IO, _IOR, _IOW, _IOWR, _IOC).

use crate::pp::{Pp, Tok};

/// Names from outside the inputs that the inputs' macros use (drm.h: DRM_RDWR, DRM_CLOEXEC for PRIME export), with
/// their x86_64 values: linux-6.6/include/uapi/asm-generic/fcntl.h:22 and :63 (both `int` in C). The probe includes
/// <fcntl.h>, so GCC checks them too.
pub const OUTSIDE: [(&str, i128); 2] = [("O_RDWR", 0o2), ("O_CLOEXEC", 0o2000000)];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CTy {
    Int,
    UInt,
    Long,
    ULong,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Val {
    pub v: i128,
    pub t: CTy,
}

impl Val {
    pub fn new(v: i128, t: CTy) -> Val {
        let v = match t {
            CTy::Int => v as i32 as i128,
            CTy::UInt => v as u32 as i128,
            CTy::Long => v as i64 as i128,
            CTy::ULong => v as u64 as i128,
        };
        Val { v, t }
    }
}

/// What expressions may look up besides macros.
pub trait Env {
    fn enumerator(&self, _name: &str) -> Option<Val> {
        None
    }
    /// sizeof of a type name given as tokens.
    fn size_of(&self, _ty: &[Tok]) -> Result<u64, String> {
        Err("sizeof needs the declarations".into())
    }
    fn cast(&self, _ty: &[Tok]) -> Option<CTy> {
        None
    }
}

struct NoEnv;
impl Env for NoEnv {}

/// Result of evaluating a macro: the value and, for ioctl numbers, the argument type's tokens (empty for _IO).
pub struct Done {
    pub val: Val,
    pub ioctl_ty: Option<Vec<Tok>>,
}

pub fn cond(pp: &Pp, toks: &[Tok]) -> Result<bool, String> {
    let mut e = Ev { pp, env: &NoEnv, toks: expand(pp, toks, &mut Vec::new())?, i: 0, ioctl_ty: None, in_if: true };
    let v = e.expr(0)?;
    Ok(v.v != 0)
}

pub fn eval(pp: &Pp, env: &dyn Env, toks: &[Tok]) -> Result<Done, String> {
    let mut e = Ev { pp, env, toks: expand(pp, toks, &mut Vec::new())?, i: 0, ioctl_ty: None, in_if: false };
    let val = e.expr(0)?;
    if e.i != e.toks.len() {
        return Err(format!("trailing tokens after expression: {:?}", &e.toks[e.i..]));
    }
    Ok(Done { val, ioctl_ty: e.ioctl_ty })
}

/// C macro expansion (no # or ##; the inputs use neither in integer macros).
fn expand(pp: &Pp, toks: &[Tok], active: &mut Vec<String>) -> Result<Vec<Tok>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        i += 1;
        let Some(name) = t.id() else {
            out.push(t.clone());
            continue;
        };
        let Some(m) = pp.get(name).filter(|_| !active.iter().any(|a| a == name)) else {
            out.push(t.clone());
            continue;
        };
        if active.len() > 64 {
            return Err("macro expansion too deep".into());
        }
        let body = match &m.params {
            None => m.body.clone(),
            Some(ps) => {
                if !toks.get(i).is_some_and(|t| t.is_p("(")) {
                    out.push(t.clone());
                    continue;
                }
                let (args, next) = split_args(toks, i)?;
                i = next;
                if args.len() != ps.len() && !(ps.is_empty() && args.len() == 1 && args[0].is_empty()) {
                    return Err(format!("{name}: {} arguments for {} parameters", args.len(), ps.len()));
                }
                let mut ex = Vec::new();
                for a in &args {
                    ex.push(expand(pp, a, active)?);
                }
                let mut b = Vec::new();
                for bt in &m.body {
                    match bt.id().and_then(|id| ps.iter().position(|p| p == id)) {
                        Some(k) => b.extend(ex[k].iter().cloned()),
                        None => b.push(bt.clone()),
                    }
                }
                b
            }
        };
        active.push(name.to_string());
        let r = expand(pp, &body, active);
        active.pop();
        out.extend(r?);
    }
    Ok(out)
}

/// Splits the arguments of a call whose '(' is at `open`; returns them and the index after ')'.
pub fn split_args(toks: &[Tok], open: usize) -> Result<(Vec<Vec<Tok>>, usize), String> {
    let (mut depth, mut args, mut cur) = (0usize, Vec::new(), Vec::new());
    for (k, t) in toks.iter().enumerate().skip(open + 1) {
        if t.is_p("(") {
            depth += 1;
        } else if t.is_p(")") {
            if depth == 0 {
                args.push(cur);
                return Ok((args, k + 1));
            }
            depth -= 1;
        } else if t.is_p(",") && depth == 0 {
            args.push(std::mem::take(&mut cur));
            continue;
        }
        cur.push(t.clone());
    }
    Err("unclosed macro call".into())
}

pub fn literal(s: &str) -> Result<Val, String> {
    let lower = s.to_ascii_lowercase();
    let digits = lower.trim_end_matches(['u', 'l']);
    let suffix = &lower[digits.len()..];
    let (radix, body) = if let Some(h) = digits.strip_prefix("0x") {
        (16, h)
    } else if digits.len() > 1 && digits.starts_with('0') {
        (8, &digits[1..])
    } else {
        (10, digits)
    };
    let v = u64::from_str_radix(body, radix).map_err(|e| format!("number {s}: {e}"))? as i128;
    let (uns, long) = (suffix.contains('u'), suffix.contains('l'));
    let fits = |t: CTy| Val::new(v, t).v == v;
    // C11 6.4.4.1: the first type of the list that can represent the value.
    let list: &[CTy] = match (uns, long, radix == 10) {
        (false, false, true) => &[CTy::Int, CTy::Long],
        (false, false, false) => &[CTy::Int, CTy::UInt, CTy::Long, CTy::ULong],
        (true, false, _) => &[CTy::UInt, CTy::ULong],
        (false, true, true) => &[CTy::Long],
        (false, true, false) => &[CTy::Long, CTy::ULong],
        (true, true, _) => &[CTy::ULong],
    };
    list.iter().copied().find(|&t| fits(t)).map(|t| Val::new(v, t)).ok_or_else(|| format!("number {s} too large"))
}

fn common(a: CTy, b: CTy) -> CTy {
    // LP64: long holds every unsigned int, so (long, unsigned int) is long.
    a.max(b)
}

const BINOPS: [(&str, u8); 18] = [
    ("||", 1),
    ("&&", 2),
    ("|", 3),
    ("^", 4),
    ("&", 5),
    ("==", 6),
    ("!=", 6),
    ("<", 7),
    (">", 7),
    ("<=", 7),
    (">=", 7),
    ("<<", 8),
    (">>", 8),
    ("+", 9),
    ("-", 9),
    ("*", 10),
    ("/", 10),
    ("%", 10),
];

struct Ev<'a> {
    pp: &'a Pp,
    env: &'a dyn Env,
    toks: Vec<Tok>,
    i: usize,
    ioctl_ty: Option<Vec<Tok>>,
    in_if: bool,
}

impl Ev<'_> {
    fn peek_p(&self, p: &str) -> bool {
        self.toks.get(self.i).is_some_and(|t| t.is_p(p))
    }

    fn expect(&mut self, p: &str) -> Result<(), String> {
        if self.peek_p(p) {
            self.i += 1;
            Ok(())
        } else {
            Err(format!("expected `{p}` at {:?}", self.toks.get(self.i)))
        }
    }

    fn expr(&mut self, min: u8) -> Result<Val, String> {
        let mut l = self.unary()?;
        loop {
            let Some(Tok::P(op)) = self.toks.get(self.i) else { break };
            let Some(&(op, prec)) = BINOPS.iter().find(|(o, _)| o == op) else { break };
            if prec <= min {
                break;
            }
            self.i += 1;
            let r = self.expr(prec)?;
            l = bin(op, l, r)?;
        }
        if min == 0 && self.peek_p("?") {
            self.i += 1;
            let a = self.expr(0)?;
            self.expect(":")?;
            let b = self.expr(0)?;
            l = if l.v != 0 { a } else { b };
        }
        Ok(l)
    }

    fn unary(&mut self) -> Result<Val, String> {
        let t = self.toks.get(self.i).cloned().ok_or("expression ends early")?;
        self.i += 1;
        match t {
            Tok::P(p) if p == "-" => self.unary().map(|v| Val::new(-v.v, v.t.max(CTy::Int))),
            Tok::P(p) if p == "+" => self.unary(),
            Tok::P(p) if p == "~" => self.unary().map(|v| Val::new(!v.v, v.t.max(CTy::Int))),
            Tok::P(p) if p == "!" => self.unary().map(|v| Val::new((v.v == 0) as i128, CTy::Int)),
            Tok::P(p) if p == "(" => {
                let close = self.toks[self.i..].iter().position(|t| t.is_p(")")).map(|k| self.i + k);
                if let Some(c) = close.and_then(|c| self.env.cast(&self.toks[self.i..c]).map(|t| (c, t))) {
                    self.i = c.0 + 1;
                    return self.unary().map(|v| Val::new(v.v, c.1));
                }
                let v = self.expr(0)?;
                self.expect(")")?;
                Ok(v)
            }
            Tok::Num(s) => literal(&s),
            Tok::Chr(c) => Ok(Val::new(c as i128, CTy::Int)),
            Tok::Id(id) => self.ident(&id),
            other => Err(format!("unexpected {other:?} in expression")),
        }
    }

    fn ident(&mut self, id: &str) -> Result<Val, String> {
        let dir = match id {
            "_IO" => Some((0, false)),
            "_IOW" => Some((1, true)),
            "_IOR" => Some((2, true)),
            "_IOWR" => Some((3, true)),
            _ => None,
        };
        if id == "sizeof" {
            self.expect("(")?;
            let (args, next) = split_args(&self.toks, self.i - 1)?;
            self.i = next;
            return Ok(Val::new(self.env.size_of(&args.concat())? as i128, CTy::ULong));
        }
        if let Some((dir, sized)) = dir {
            if !self.peek_p("(") {
                return Err(format!("{id} without arguments"));
            }
            let (args, next) = split_args(&self.toks, self.i)?;
            self.i = next;
            let want = if sized { 3 } else { 2 };
            if args.len() != want {
                return Err(format!("{id} takes {want} arguments"));
            }
            let ty = eval(self.pp, self.env, &args[0])?.val.v;
            let nr = eval(self.pp, self.env, &args[1])?.val.v;
            let size = if sized { self.env.size_of(&args[2])? as i128 } else { 0 };
            if !(0..0x100).contains(&ty) || !(0..0x100).contains(&nr) || size >= 1 << 14 {
                return Err(format!("{id}: type {ty:#x}, nr {nr:#x} or size {size} out of range"));
            }
            if self.ioctl_ty.is_none() {
                self.ioctl_ty = Some(if sized { args[2].clone() } else { Vec::new() });
            }
            return Ok(Val::new(dir << 30 | size << 16 | ty << 8 | nr, CTy::UInt));
        }
        if let Some(v) = self.env.enumerator(id) {
            return Ok(v);
        }
        if let Some((_, v)) = OUTSIDE.iter().find(|(n, _)| *n == id) {
            return Ok(Val::new(*v, CTy::Int));
        }
        if self.in_if {
            return Ok(Val::new(0, CTy::Int));
        }
        Err(format!("unknown name {id}"))
    }
}

fn bin(op: &str, l: Val, r: Val) -> Result<Val, String> {
    let shift = op == "<<" || op == ">>";
    let t = if shift { l.t.max(CTy::Int) } else { common(l.t.max(CTy::Int), r.t.max(CTy::Int)) };
    let (a, b) = (Val::new(l.v, t).v, Val::new(r.v, t).v);
    let cmp = |c: bool| Ok(Val::new(c as i128, CTy::Int));
    match op {
        "||" => cmp(l.v != 0 || r.v != 0),
        "&&" => cmp(l.v != 0 && r.v != 0),
        "==" => cmp(a == b),
        "!=" => cmp(a != b),
        "<" => cmp(a < b),
        ">" => cmp(a > b),
        "<=" => cmp(a <= b),
        ">=" => cmp(a >= b),
        "|" => Ok(Val::new(a | b, t)),
        "^" => Ok(Val::new(a ^ b, t)),
        "&" => Ok(Val::new(a & b, t)),
        "<<" | ">>" if !(0..64).contains(&r.v) => Err(format!("shift by {}", r.v)),
        "<<" => Ok(Val::new(a << r.v, t)),
        ">>" => Ok(Val::new(a >> r.v, t)),
        "+" => Ok(Val::new(a + b, t)),
        "-" => Ok(Val::new(a - b, t)),
        "*" => Ok(Val::new(a * b, t)),
        "/" | "%" if b == 0 => Err("division by zero".into()),
        "/" => Ok(Val::new(a / b, t)),
        "%" => Ok(Val::new(a % b, t)),
        _ => Err(format!("operator {op}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_follow_c_types() {
        assert_eq!(literal("5").unwrap(), Val { v: 5, t: CTy::Int });
        assert_eq!(literal("0x80000000").unwrap().t, CTy::UInt);
        assert_eq!(literal("2147483648").unwrap().t, CTy::Long);
        assert_eq!(literal("0x40000000U").unwrap().t, CTy::UInt);
        assert_eq!(literal("1ULL").unwrap().t, CTy::ULong);
        assert_eq!(literal("010").unwrap().v, 8);
    }

    #[test]
    fn int_shift_overflow_wraps_like_gcc() {
        let v = bin("<<", literal("1").unwrap(), literal("31").unwrap()).unwrap();
        assert_eq!(v, Val { v: -2147483648, t: CTy::Int });
    }
}
