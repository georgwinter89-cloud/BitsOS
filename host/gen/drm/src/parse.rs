//! Declarations (struct, union, enum, typedef) and their x86_64 System V layout.

use std::collections::HashMap;

use crate::eval::{self, CTy, Env, Val};
use crate::pp::{Pp, Tok};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prim {
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
    U64,
    I64,
}

impl Prim {
    pub fn size(self) -> u64 {
        match self {
            Prim::U8 | Prim::I8 => 1,
            Prim::U16 | Prim::I16 => 2,
            Prim::U32 | Prim::I32 => 4,
            Prim::U64 | Prim::I64 => 8,
        }
    }
    pub fn rust(self) -> &'static str {
        ["u8", "i8", "u16", "i16", "u32", "i32", "u64", "i64"][self as usize]
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Ty {
    Prim(Prim),
    /// A user pointer: 8 bytes, carried as u64.
    Ptr,
    Arr(Box<Ty>, u64),
    Agg(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// `struct tag` / `union tag`, a file-scope name in C.
    Tagged(String),
    /// Untagged aggregate that is the type of the named member `path` (e.g. "data[0]") of `parent`.
    Inline { parent: usize, path: String },
    /// Untagged member without a name; its members belong to the parent in C.
    Anon { parent: usize, k: usize },
}

pub struct Member {
    pub name: Option<String>, // None: anonymous aggregate member
    pub ty: Ty,
    pub offset: u64,
}

pub struct Agg {
    pub rust: String,
    pub kind: Kind,
    pub union: bool,
    pub members: Vec<Member>,
    pub size: u64,
    pub align: u64,
    pub file: usize,
    pub defined: bool,
    anons: usize,
}

pub struct Enumerator {
    pub name: String,
    pub val: Val,
    pub enum_name: String,
    pub file: usize,
}

pub struct Typedef {
    pub name: String,
    pub ty: Ty,
    pub file: usize,
}

pub struct Model<'p> {
    pub pp: &'p Pp,
    pub aggs: Vec<Agg>,
    tags: HashMap<String, usize>,
    pub enumerators: Vec<Enumerator>,
    enum_vals: HashMap<String, Val>,
    enum_signed: HashMap<String, bool>,
    pub typedefs: Vec<Typedef>,
    td: HashMap<String, Ty>,
}

/// Types the inputs take from linux/types.h and the compiler (x86_64, LP64).
fn builtin(name: &str) -> Option<Prim> {
    Some(match name {
        "__u8" => Prim::U8,
        "__s8" => Prim::I8,
        "__u16" | "__le16" | "__be16" => Prim::U16,
        "__s16" => Prim::I16,
        "__u32" | "__le32" | "__be32" => Prim::U32,
        "__s32" => Prim::I32,
        "__u64" | "__le64" | "__be64" | "__aligned_u64" | "__kernel_size_t" | "size_t" => Prim::U64,
        "__s64" | "__kernel_ssize_t" => Prim::I64,
        _ => return None,
    })
}

const QUALIFIERS: [&str; 4] = ["const", "volatile", "__volatile__", "__user"];
const BASIC: [&str; 6] = ["unsigned", "signed", "char", "short", "int", "long"];

fn basic(words: &[&str]) -> Result<Prim, String> {
    let has = |w: &str| words.contains(&w);
    let uns = has("unsigned");
    Ok(if has("char") {
        // Plain char is signed on x86_64; as Rust bytes it is u8 (same layout), see the notes.
        if has("signed") { Prim::I8 } else { Prim::U8 }
    } else if has("short") {
        if uns { Prim::U16 } else { Prim::I16 }
    } else if has("long") {
        if uns { Prim::U64 } else { Prim::I64 }
    } else if has("int") || uns || has("signed") {
        if uns { Prim::U32 } else { Prim::I32 }
    } else {
        return Err(format!("unknown basic type {words:?}"));
    })
}

impl Model<'_> {
    pub fn size_align(&self, t: &Ty) -> (u64, u64) {
        match t {
            Ty::Prim(p) => (p.size(), p.size()),
            Ty::Ptr => (8, 8),
            Ty::Arr(e, n) => {
                let (s, a) = self.size_align(e);
                (s * n, a)
            }
            Ty::Agg(i) => (self.aggs[*i].size, self.aggs[*i].align),
        }
    }

    pub fn tag(&self, kw: &str, name: &str) -> Option<usize> {
        self.tags.get(&format!("{kw} {name}")).copied()
    }

    /// Parses a type name (cast or sizeof/ioctl argument): specifiers plus optional '*'s.
    pub fn type_name(&self, toks: &[Tok]) -> Result<Ty, String> {
        let mut words = Vec::new();
        let mut ty = None;
        let mut i = 0;
        while i < toks.len() {
            match &toks[i] {
                Tok::Id(w) if QUALIFIERS.contains(&w.as_str()) => {}
                Tok::Id(w) if w == "struct" || w == "union" => {
                    let n = toks.get(i + 1).and_then(|t| t.id()).ok_or("struct without tag")?;
                    let a = self.tag(w, n).ok_or_else(|| format!("unknown {w} {n}"))?;
                    if !self.aggs[a].defined {
                        return Err(format!("{w} {n} is incomplete"));
                    }
                    ty = Some(Ty::Agg(a));
                    i += 1;
                }
                Tok::Id(w) if w == "enum" => {
                    let n = toks.get(i + 1).and_then(|t| t.id()).unwrap_or("");
                    ty = Some(Ty::Prim(self.enum_prim(n)));
                    i += 1;
                }
                Tok::Id(w) if BASIC.contains(&w.as_str()) => words.push(w.as_str()),
                Tok::Id(w) if builtin(w).is_some() => ty = builtin(w).map(Ty::Prim),
                Tok::Id(w) if self.td.contains_key(w) => ty = self.td.get(w).cloned(),
                t if t.is_p("*") => return Ok(Ty::Ptr),
                t => return Err(format!("unexpected {t:?} in type name")),
            }
            i += 1;
        }
        match ty {
            Some(t) => Ok(t),
            None => basic(&words).map(Ty::Prim),
        }
    }

    fn enum_prim(&self, name: &str) -> Prim {
        if self.enum_signed.get(name).copied().unwrap_or(false) { Prim::I32 } else { Prim::U32 }
    }
}

impl Env for Model<'_> {
    fn enumerator(&self, name: &str) -> Option<Val> {
        self.enum_vals.get(name).copied()
    }
    fn size_of(&self, ty: &[Tok]) -> Result<u64, String> {
        self.type_name(ty).map(|t| self.size_align(&t).0)
    }
    fn cast(&self, ty: &[Tok]) -> Option<CTy> {
        match self.type_name(ty).ok()? {
            Ty::Prim(Prim::U64) | Ty::Ptr => Some(CTy::ULong),
            Ty::Prim(Prim::I64) => Some(CTy::Long),
            Ty::Prim(Prim::U32) => Some(CTy::UInt),
            Ty::Prim(_) => Some(CTy::Int),
            _ => None,
        }
    }
}

/// Where a parsed aggregate body sits: its parent and the name prefix for untagged types.
#[derive(Clone)]
struct Ctx {
    parent: Option<usize>,
    named: String, // Rust name of the nearest named ancestor
}

struct P<'m, 'p> {
    m: &'m mut Model<'p>,
    t: &'m [(Tok, usize)],
    i: usize,
}

pub fn parse<'p>(pp: &'p Pp) -> Result<Model<'p>, String> {
    let mut m = Model {
        pp,
        aggs: Vec::new(),
        tags: HashMap::new(),
        enumerators: Vec::new(),
        enum_vals: HashMap::new(),
        enum_signed: HashMap::new(),
        typedefs: Vec::new(),
        td: HashMap::new(),
    };
    let mut p = P { m: &mut m, t: &pp.code, i: 0 };
    while p.i < p.t.len() {
        p.top().map_err(|e| format!("{e} (near token {} {:?})", p.i, p.t.get(p.i).map(|t| &t.0)))?;
    }
    Ok(m)
}

impl P<'_, '_> {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i).map(|t| &t.0)
    }
    fn file(&self) -> usize {
        self.t.get(self.i).or(self.t.last()).map_or(0, |t| t.1)
    }
    fn next(&mut self) -> Result<Tok, String> {
        let t = self.peek().cloned().ok_or("unexpected end of input")?;
        self.i += 1;
        Ok(t)
    }
    fn eat(&mut self, p: &str) -> bool {
        let y = self.peek().is_some_and(|t| t.is_p(p));
        self.i += y as usize;
        y
    }
    fn expect(&mut self, p: &str) -> Result<(), String> {
        if self.eat(p) { Ok(()) } else { Err(format!("expected `{p}`, found {:?}", self.peek())) }
    }

    fn top(&mut self) -> Result<(), String> {
        if self.peek().and_then(|t| t.id()) == Some("typedef") {
            self.i += 1;
            let file = self.file();
            let base = self.spec(&Ctx { parent: None, named: String::new() })?;
            let (name, ty) = self.declarator(base)?;
            self.expect(";")?;
            self.m.td.insert(name.clone(), ty.clone());
            self.m.typedefs.push(Typedef { name, ty, file });
            return Ok(());
        }
        self.spec(&Ctx { parent: None, named: String::new() })?;
        self.expect(";")
    }

    /// Type specifier; defines an aggregate or enum body if one follows.
    fn spec(&mut self, cx: &Ctx) -> Result<Ty, String> {
        let mut words: Vec<String> = Vec::new();
        loop {
            let Some(Tok::Id(w)) = self.peek().cloned() else { break };
            if QUALIFIERS.contains(&w.as_str()) {
                self.i += 1;
            } else if w == "struct" || w == "union" {
                self.i += 1;
                return self.aggregate(w == "union", cx);
            } else if w == "enum" {
                self.i += 1;
                return self.enumeration().map(Ty::Prim);
            } else if BASIC.contains(&w.as_str()) {
                words.push(w);
                self.i += 1;
            } else if words.is_empty() && (builtin(&w).is_some() || self.m.td.contains_key(&w)) {
                self.i += 1;
                return Ok(builtin(&w).map(Ty::Prim).or_else(|| self.m.td.get(&w).cloned()).unwrap_or(Ty::Ptr));
            } else if words.is_empty() && w == "void" {
                self.i += 1;
                return Ok(Ty::Prim(Prim::U8)); // only valid behind '*'
            } else {
                break;
            }
        }
        let w: Vec<&str> = words.iter().map(String::as_str).collect();
        basic(&w).map(Ty::Prim)
    }

    fn aggregate(&mut self, union: bool, cx: &Ctx) -> Result<Ty, String> {
        let kw = if union { "union" } else { "struct" };
        let tag = self.peek().and_then(|t| t.id()).map(str::to_string);
        if tag.is_some() {
            self.i += 1;
        }
        let has_body = self.peek().is_some_and(|t| t.is_p("{"));
        let file = self.file();
        let idx = match &tag {
            Some(n) => match self.m.tag(kw, n) {
                Some(i) => i,
                None => {
                    let i = self.new_agg(n.clone(), Kind::Tagged(n.clone()), union, file);
                    self.m.tags.insert(format!("{kw} {n}"), i);
                    i
                }
            },
            None if !has_body => return Err(format!("{kw} without tag or body")),
            // Kind and name of an untagged body are fixed by the caller once the declarator is known.
            None => self.new_agg(String::new(), Kind::Anon { parent: cx.parent.unwrap_or(usize::MAX), k: 0 }, union, file),
        };
        if !has_body {
            return Ok(Ty::Agg(idx));
        }
        if self.m.aggs[idx].defined {
            return Err(format!("{kw} {} defined twice", self.m.aggs[idx].rust));
        }
        self.i += 1;
        let inner = Ctx { parent: Some(idx), named: if tag.is_some() { self.m.aggs[idx].rust.clone() } else { cx.named.clone() } };
        let mut members = Vec::new();
        while !self.eat("}") {
            let base = self.spec(&inner)?;
            if self.eat(";") {
                let Ty::Agg(a) = base else { return Err("member without name".into()) };
                if !matches!(self.m.aggs[a].kind, Kind::Anon { .. }) {
                    return Err("tagged aggregate member without name".into());
                }
                let k = self.m.aggs[idx].anons;
                self.m.aggs[idx].anons += 1;
                self.m.aggs[a].kind = Kind::Anon { parent: idx, k };
                self.m.aggs[a].rust = format!("{}__anon{k}", inner.named);
                members.push(Member { name: None, ty: base, offset: 0 });
                continue;
            }
            loop {
                let (name, ty) = self.declarator(base.clone())?;
                if let Ty::Agg(a) = base_agg(&ty) {
                    if matches!(self.m.aggs[a].kind, Kind::Anon { .. }) && self.m.aggs[a].rust.is_empty() {
                        let path = if matches!(ty, Ty::Arr(..)) { format!("{name}[0]") } else { name.clone() };
                        self.m.aggs[a].kind = Kind::Inline { parent: idx, path };
                        self.m.aggs[a].rust = format!("{}__{name}", inner.named);
                    }
                }
                members.push(Member { name: Some(name), ty, offset: 0 });
                if !self.eat(",") {
                    break;
                }
            }
            self.expect(";")?;
        }
        self.layout(idx, members);
        Ok(Ty::Agg(idx))
    }

    fn new_agg(&mut self, rust: String, kind: Kind, union: bool, file: usize) -> usize {
        let a = Agg { rust, kind, union, members: Vec::new(), size: 0, align: 1, file, defined: false, anons: 0 };
        self.m.aggs.push(a);
        self.m.aggs.len() - 1
    }

    fn layout(&mut self, idx: usize, mut members: Vec<Member>) {
        let union = self.m.aggs[idx].union;
        let (mut end, mut align) = (0u64, 1u64);
        for mb in &mut members {
            let (s, a) = self.m.size_align(&mb.ty);
            align = align.max(a);
            mb.offset = if union { 0 } else { end.div_ceil(a) * a };
            end = if union { end.max(s) } else { mb.offset + s };
        }
        let ag = &mut self.m.aggs[idx];
        ag.size = end.div_ceil(align) * align;
        ag.align = align;
        ag.members = members;
        ag.defined = true;
    }

    fn enumeration(&mut self) -> Result<Prim, String> {
        let tag = self.peek().and_then(|t| t.id()).map(str::to_string);
        if tag.is_some() {
            self.i += 1;
        }
        let tag = tag.unwrap_or_default();
        if !self.eat("{") {
            return Ok(self.m.enum_prim(&tag));
        }
        let file = self.file();
        let mut next = Val::new(0, CTy::Int);
        let mut signed = false;
        let first = self.m.enumerators.len();
        while !self.eat("}") {
            let name = self.next()?.id().ok_or("enumerator name expected")?.to_string();
            if self.eat("=") {
                let start = self.i;
                let mut depth = 0i32;
                while let Some(t) = self.peek() {
                    if depth == 0 && (t.is_p(",") || t.is_p("}")) {
                        break;
                    }
                    depth += t.is_p("(") as i32 - t.is_p(")") as i32;
                    self.i += 1;
                }
                let toks: Vec<Tok> = self.t[start..self.i].iter().map(|t| t.0.clone()).collect();
                next = eval::eval(self.m.pp, &*self.m, &toks)?.val;
            }
            signed |= next.v < 0;
            self.m.enum_vals.insert(name.clone(), next);
            self.m.enumerators.push(Enumerator { name, val: next, enum_name: tag.clone(), file });
            next = Val::new(next.v + 1, next.t);
            if !self.eat(",") {
                self.expect("}")?;
                break;
            }
        }
        // C gives enumerators type int; the enum type is unsigned int unless a value is negative (GCC).
        for e in &mut self.m.enumerators[first..] {
            e.val = Val::new(e.val.v, CTy::Int);
        }
        if !tag.is_empty() {
            self.m.enum_signed.insert(tag, signed);
        }
        Ok(if signed { Prim::I32 } else { Prim::U32 })
    }

    /// `*`* name (`[` expr `]`)*
    fn declarator(&mut self, base: Ty) -> Result<(String, Ty), String> {
        let mut ptr = false;
        while self.eat("*") {
            ptr = true;
            while self.peek().and_then(|t| t.id()).is_some_and(|w| QUALIFIERS.contains(&w)) {
                self.i += 1;
            }
        }
        let name = self.next()?.id().ok_or("declarator name expected")?.to_string();
        let mut dims = Vec::new();
        while self.eat("[") {
            let start = self.i;
            while self.peek().is_some_and(|t| !t.is_p("]")) {
                self.i += 1;
            }
            let toks: Vec<Tok> = self.t[start..self.i].iter().map(|t| t.0.clone()).collect();
            self.expect("]")?;
            if toks.is_empty() {
                return Err(format!("flexible array member {name} is not supported"));
            }
            let n = eval::eval(self.m.pp, &*self.m, &toks)?.val.v;
            dims.push(u64::try_from(n).map_err(|_| format!("{name}: negative array size"))?);
        }
        let mut ty = if ptr { Ty::Ptr } else { base };
        for n in dims.into_iter().rev() {
            ty = Ty::Arr(Box::new(ty), n);
        }
        Ok((name, ty))
    }
}

pub fn base_agg(t: &Ty) -> Ty {
    match t {
        Ty::Arr(e, _) => base_agg(e),
        t => t.clone(),
    }
}
