//! Rust's v0 symbol mangling (`_R…`, the toolchain's default now), shown the way binutils and rustc-demangle
//! show it without hashes: `crate::module::function`, `<Type as Trait>::method`, `{closure#0}`, generic
//! arguments in `<…>`. A name it cannot read stays as it is (None), so a report never shows half a name.

/// The readable name of a v0 symbol, None if it is not one or does not parse.
pub fn demangle(s: &str) -> Option<String> {
    let body = s.strip_prefix("_R").or_else(|| s.strip_prefix("__R"))?;
    // LLVM's `.llvm.<hash>` and similar suffixes come after the symbol.
    let body = body.split_once('.').map_or(body, |(b, _)| b);
    let mut p = P { s: body.as_bytes(), i: 0, out: String::new(), depth: 0, in_type: 0 };
    if p.peek().is_some_and(|c| c.is_ascii_digit()) {
        p.decimal()?;
    }
    p.path()?;
    Some(p.out)
}

struct P<'a> {
    s: &'a [u8],
    i: usize,
    out: String,
    depth: u32,
    /// Inside a type: generic arguments print as `<…>`; in a value path as `::<…>` (as rustc-demangle).
    in_type: u32,
}

const MAX_DEPTH: u32 = 64;
const MAX_OUT: usize = 4096;

impl P<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }
    fn next(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.i += 1;
        Some(c)
    }
    fn eat(&mut self, c: u8) -> bool {
        let yes = self.peek() == Some(c);
        if yes {
            self.i += 1;
        }
        yes
    }
    fn push(&mut self, t: &str) -> Option<()> {
        self.out.push_str(t);
        (self.out.len() <= MAX_OUT).then_some(())
    }
    /// `0` or `[1-9][0-9]*`: a leading 0 is a whole number (`00` is two of them).
    fn decimal(&mut self) -> Option<usize> {
        let start = self.i;
        if self.eat(b'0') {
            return Some(0);
        }
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        std::str::from_utf8(&self.s[start..self.i]).ok()?.parse().ok()
    }
    /// `_` = 0, `<digits>_` = digits + 1 (digits in 0-9a-zA-Z).
    fn base62(&mut self) -> Option<u64> {
        if self.eat(b'_') {
            return Some(0);
        }
        let mut v: u64 = 0;
        loop {
            let c = self.next()?;
            let d = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'z' => c - b'a' + 10,
                b'A'..=b'Z' => c - b'A' + 36,
                b'_' => return v.checked_add(1),
                _ => return None,
            };
            v = v.checked_mul(62)?.checked_add(d as u64)?;
        }
    }
    fn disambiguator(&mut self) -> Option<u64> {
        if self.eat(b's') { self.base62().map(|v| v + 1) } else { Some(0) }
    }
    /// `[u] <len> [_] <bytes>` (punycode names are shown as they are).
    fn ident(&mut self) -> Option<&str> {
        self.eat(b'u');
        let n = self.decimal()?;
        self.eat(b'_');
        let b = self.s.get(self.i..self.i + n)?;
        self.i += n;
        std::str::from_utf8(b).ok()
    }
    /// Runs `f` at the position a back-reference names, then comes back.
    fn backref(&mut self, f: fn(&mut Self) -> Option<()>) -> Option<()> {
        let at = self.base62()? as usize;
        if at >= self.i {
            return None;
        }
        let back = core::mem::replace(&mut self.i, at);
        let r = self.nested(f);
        self.i = back;
        r
    }
    fn nested(&mut self, f: fn(&mut Self) -> Option<()>) -> Option<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return None;
        }
        let r = f(self);
        self.depth -= 1;
        r
    }
    fn path(&mut self) -> Option<()> {
        self.nested(Self::path_inner)
    }
    fn path_inner(&mut self) -> Option<()> {
        match self.next()? {
            b'C' => {
                self.disambiguator()?;
                let n = self.ident()?.to_string();
                self.push(&n)
            }
            b'N' => {
                let ns = self.next()?;
                self.path()?;
                let dis = self.disambiguator()?;
                let n = self.ident()?.to_string();
                if ns.is_ascii_lowercase() {
                    // An internal namespace with no name (a tuple struct's constructor) adds nothing.
                    if n.is_empty() {
                        return Some(());
                    }
                    self.push("::")?;
                    return self.push(&n);
                }
                let kind = match ns {
                    b'C' => "closure".to_string(),
                    b'S' => "shim".to_string(),
                    _ => (ns as char).to_string(),
                };
                let name = if n.is_empty() { String::new() } else { format!(":{n}") };
                self.push(&format!("::{{{kind}{name}#{dis}}}"))
            }
            b'M' => {
                self.impl_path()?;
                self.push("<")?;
                self.ty()?;
                self.push(">")
            }
            b'X' => {
                self.impl_path()?;
                self.push("<")?;
                self.ty()?;
                self.push(" as ")?;
                self.type_path()?;
                self.push(">")
            }
            b'Y' => {
                self.push("<")?;
                self.ty()?;
                self.push(" as ")?;
                self.type_path()?;
                self.push(">")
            }
            b'I' => {
                self.path()?;
                self.generic_args()
            }
            b'B' => self.backref(Self::path_inner),
            _ => None,
        }
    }
    fn type_path(&mut self) -> Option<()> {
        self.in_type += 1;
        let r = self.path();
        self.in_type -= 1;
        r
    }
    /// The impl's own path is not shown (as binutils without --verbose): parsed into nothing.
    fn impl_path(&mut self) -> Option<()> {
        self.disambiguator()?;
        let len = self.out.len();
        self.path()?;
        self.out.truncate(len);
        Some(())
    }
    /// `{<generic-arg>} E` after a path: `<a, b>` (lifetimes left out).
    fn generic_args(&mut self) -> Option<()> {
        let open = self.out.len();
        self.push(if self.in_type == 0 { "::<" } else { "<" })?;
        self.in_type += 1;
        let mut first = true;
        while !self.eat(b'E') {
            if self.eat(b'L') {
                self.base62()?;
                continue;
            }
            if !first {
                self.push(", ")?;
            }
            first = false;
            if self.eat(b'K') { self.konst()? } else { self.ty()? }
        }
        self.in_type -= 1;
        if first {
            // Only lifetimes: nothing to show.
            self.out.truncate(open);
            return Some(());
        }
        self.push(">")
    }
    fn ty(&mut self) -> Option<()> {
        self.in_type += 1;
        let r = self.nested(Self::ty_inner);
        self.in_type -= 1;
        r
    }
    fn ty_inner(&mut self) -> Option<()> {
        let c = self.peek()?;
        if let Some(b) = basic(c) {
            self.i += 1;
            return self.push(b);
        }
        self.i += 1;
        match c {
            b'R' | b'Q' => {
                if self.eat(b'L') {
                    self.base62()?;
                }
                self.push(if c == b'R' { "&" } else { "&mut " })?;
                self.ty()
            }
            b'P' | b'O' => {
                self.push(if c == b'P' { "*const " } else { "*mut " })?;
                self.ty()
            }
            b'A' => {
                self.push("[")?;
                self.ty()?;
                self.push("; ")?;
                self.konst()?;
                self.push("]")
            }
            b'S' => {
                self.push("[")?;
                self.ty()?;
                self.push("]")
            }
            b'T' => {
                self.push("(")?;
                let mut n = 0;
                while !self.eat(b'E') {
                    if n > 0 {
                        self.push(", ")?;
                    }
                    self.ty()?;
                    n += 1;
                }
                self.push(if n == 1 { ",)" } else { ")" })
            }
            b'F' => self.fn_sig(),
            b'D' => self.dyn_bounds(),
            b'B' => self.backref(Self::ty_inner),
            _ => {
                self.i -= 1;
                self.path()
            }
        }
    }
    fn fn_sig(&mut self) -> Option<()> {
        if self.eat(b'G') {
            self.base62()?;
        }
        let unsafe_ = self.eat(b'U');
        if unsafe_ {
            self.push("unsafe ")?;
        }
        if self.eat(b'K') {
            let abi = if self.eat(b'C') { "C".to_string() } else { self.ident()?.replace('_', "-") };
            self.push(&format!("extern \"{abi}\" "))?;
        }
        self.push("fn(")?;
        let mut n = 0;
        while !self.eat(b'E') {
            if n > 0 {
                self.push(", ")?;
            }
            self.ty()?;
            n += 1;
        }
        self.push(")")?;
        if self.eat(b'u') {
            return Some(());
        }
        self.push(" -> ")?;
        self.ty()
    }
    fn dyn_bounds(&mut self) -> Option<()> {
        if self.eat(b'G') {
            self.base62()?;
        }
        self.push("dyn ")?;
        let mut n = 0;
        while !self.eat(b'E') {
            if n > 0 {
                self.push(" + ")?;
            }
            let start = self.out.len();
            self.path()?;
            // Associated types join the trait's own arguments: `Fn<(A,), Output = B>`.
            let mut open = self.out[start..].ends_with('>');
            while self.eat(b'p') {
                let name = self.ident()?.to_string();
                if open {
                    self.out.pop();
                    self.push(&format!(", {name} = "))?;
                } else {
                    self.push(&format!("<{name} = "))?;
                }
                self.ty()?;
                self.push(">")?;
                open = true;
            }
            n += 1;
        }
        if !self.eat(b'L') {
            return None;
        }
        self.base62().map(|_| ())
    }
    /// A const generic argument: integers, bools, chars; `_` for a placeholder.
    fn konst(&mut self) -> Option<()> {
        if self.eat(b'p') {
            return self.push("_");
        }
        if self.eat(b'B') {
            return self.backref(Self::konst);
        }
        let t = self.next()?;
        let neg = self.eat(b'n');
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
            self.i += 1;
        }
        let hex = std::str::from_utf8(&self.s[start..self.i]).ok()?;
        if !self.eat(b'_') {
            return None;
        }
        let v = if hex.is_empty() { 0 } else { u128::from_str_radix(hex, 16).ok()? };
        let text = match t {
            b'b' => (if v == 0 { "false" } else { "true" }).to_string(),
            b'c' => format!("{:?}", char::from_u32(u32::try_from(v).ok()?)?),
            _ if basic(t).is_some() => format!("{}{v}", if neg { "-" } else { "" }),
            _ => return None,
        };
        self.push(&text)
    }
}

fn basic(c: u8) -> Option<&'static str> {
    Some(match c {
        b'a' => "i8",
        b'b' => "bool",
        b'c' => "char",
        b'd' => "f64",
        b'e' => "str",
        b'f' => "f32",
        b'h' => "u8",
        b'i' => "isize",
        b'j' => "usize",
        b'l' => "i32",
        b'm' => "u32",
        b'n' => "i128",
        b'o' => "u128",
        b's' => "i16",
        b't' => "u16",
        b'u' => "()",
        b'v' => "...",
        b'x' => "i64",
        b'y' => "u64",
        b'z' => "!",
        b'p' => "_",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::demangle;

    // Real symbols of BitsOS programs; the answers are binutils' (`c++filt --format=rust`) without crate hashes,
    // lifetimes and the type suffix of const arguments.
    #[test]
    fn real_names() {
        let cases = [
            ("_RNvCsehNTdsMa8nN_8profbusy17profbusy_hot_loop", "profbusy::profbusy_hot_loop"),
            ("_RNvCslv2iGpLBxFJ_8bitsprof3run.llvm.1234", "bitsprof::run"),
            ("_RINvMNtCsifRdLFh1usa_4core3stre12trim_matchesNvMNtNtB5_4char7methodsc13is_whitespaceECsdxGOV0mSivN_3std", "<str>::trim_matches::<<char>::is_whitespace>"),
            ("_RNCNCNCNvNtCsdxGOV0mSivN_3std2rt19lang_start_internal00s_0B9_", "std::rt::lang_start_internal::{closure#0}::{closure#0}::{closure#1}"),
            (
                "_RNSNvYNCNCNvNtNtCsdxGOV0mSivN_3std3sys9backtrace10__print_fmts_00INtNtNtCsifRdLFh1usa_4core3ops8function6FnOnceTRNtNtNtBe_12backtrace_rs9symbolize6SymbolEE9call_once6vtableBe_",
                "<std::sys::backtrace::_print_fmt::{closure#1}::{closure#0} as core::ops::function::FnOnce<(&std::backtrace_rs::symbolize::Symbol,)>>::call_once::{shim:vtable#0}",
            ),
            (
                "_RINvNtCsifRdLFh1usa_4core3ptr9drop_glueINtNtCskCQYDStZNxl_5alloc5boxed3BoxDG0_INtNtNtB4_3ops8function2FnTRL1_INtNtCsdxGOV0mSivN_3std5panic13PanicHookInfoL0_EEEp6OutputuNtNtB4_6marker4SendNtB2I_4SyncEL_EEB1O_",
                "core::ptr::drop_glue::<alloc::boxed::Box<dyn core::ops::function::Fn<(&std::panic::PanicHookInfo,), Output = ()> + core::marker::Send + core::marker::Sync>>",
            ),
            ("_RINvMs2_NtCsevLNFiNqfJP_4core3fmtNtB6_9Arguments3newKj22_Kj2_ECs76D6rwIEu7F_8bitsprof", "<core::fmt::Arguments>::new::<34, 2>"),
        ];
        for (m, want) in cases {
            assert_eq!(demangle(m).as_deref(), Some(want), "{m}");
        }
    }

    #[test]
    fn not_v0_or_broken() {
        assert_eq!(demangle("memcpy"), None);
        assert_eq!(demangle("_ZN8bitsprof3run17h0123456789abcdefE"), None);
        assert_eq!(demangle("_RNvC"), None);
        assert_eq!(demangle("_RNvCs_3std99toolong"), None);
        // A back-reference to itself ends at the depth limit instead of looping.
        assert_eq!(demangle("_RB_"), None);
    }
}
