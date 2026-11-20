//! GM-6w: the power socket's requests from the desktop (sofa-mode.md section 7 row "Neu starten / Ausschalten",
//! desktop.md section 0 point 9), the parts that need no system calls (unit-tested on the host).
//!
//! A request line is a JSON object `{"op":"reboot"|"poweroff"|"status","input":"physical"}` or one of UL-7's plain
//! words (`restart`, `reboot`, `poweroff`, `halt`, `status`). A restart or power-off goes ahead only for the
//! process the kernel names as the holder of fb0's compositor grant (`/proc/bitsos/compositor`, the check
//! install.sock makes for MC-9a, INT-W17) and only when the request says real input asked for it
//! (`"input":"physical"`). That statement is advisory here: bitscomp checks the press itself (EVIOC_BITSOS_PHYSICAL,
//! the event's injected flag) before it sends it; for a power-off the kernel judges once more (R8f,
//! `/proc/bitsos/power` and reboot(2)).

use crate::power::{Action, Request};

pub const COMPOSITOR_HOLDER: &str = "/proc/bitsos/compositor";
/// The compositor's statement that a physical key, click or pad press asked for the request.
pub const INPUT_PHYSICAL: &str = "physical";

/// One parsed request line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Line {
    pub request: Request,
    /// `"input":"physical"` was in the request (plain words never carry it).
    pub physical: bool,
    /// The request came as JSON, so the answer goes back as JSON.
    pub json: bool,
}

/// A request line: JSON when it starts with `{`, else UL-7's plain words. Err: why it was not understood.
pub fn parse_line(line: &str) -> Result<Line, String> {
    let t = line.trim();
    if !t.starts_with('{') {
        return crate::power::parse_request(t)
            .map(|request| Line { request, physical: false, json: false })
            .ok_or_else(|| String::from("unknown request (restart, poweroff or status)"));
    }
    let fields = object(t).ok_or_else(|| String::from("the request is not one JSON object"))?;
    let get = |k: &str| fields.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_deref());
    let op = match get("op") {
        Some(Some(o)) => Some(o),
        Some(None) => return Err(String::from("\"op\" is not a string")),
        None => None,
    };
    let request = match op {
        Some("reboot" | "restart") => Request::Do(Action::Restart),
        Some("poweroff") => Request::Do(Action::PowerOff),
        Some("status") => Request::Status,
        Some(o) => return Err(format!("unknown op {o:?} (reboot, poweroff or status)")),
        None => return Err(String::from("the request has no \"op\"")),
    };
    Ok(Line { request, physical: get("input") == Some(Some(INPUT_PHYSICAL)), json: true })
}

/// What a kernel `holder=` file says: `Some(Some(pid))`, `Some(None)` for `holder=none`, None for anything else
/// (as bitsinstall's `holder_of`; init depends on usys only).
pub fn holder_of(text: &str) -> Option<Option<u32>> {
    let v = text.trim().strip_prefix("holder=")?;
    if v == "none" {
        return Some(None);
    }
    v.parse::<u32>().ok().filter(|&p| p != 0).map(Some)
}

/// Why a restart or power-off was refused. `kind` keys the log's thinning of repeats, so a refusal of one kind
/// never hides the first refusal of another (B6).
#[derive(Debug)]
pub struct Refusal {
    pub kind: &'static str,
    pub why: String,
}

/// May `pid` restart or power off now? `holder` is the text of `COMPOSITOR_HOLDER` (None: it could not be read,
/// so no compositor is known and the request is refused). Ok: the words for the log.
pub fn do_allowed(pid: u32, holder: Option<&str>, line: &Line) -> Result<String, Refusal> {
    let no = |kind, why| Err(Refusal { kind, why });
    let Some(text) = holder else {
        return no("powerholder", format!("{COMPOSITOR_HOLDER} cannot be read, so no compositor is known"));
    };
    match holder_of(text) {
        Some(Some(h)) if h == pid => {}
        Some(Some(h)) => return no("powerholder", format!("pid {pid} does not hold the compositor grant (pid {h} does)")),
        Some(None) => return no("powerholder", String::from("no process holds the compositor grant")),
        None => return no("powerholder", format!("{COMPOSITOR_HOLDER} not understood: {:?}", text.trim())),
    }
    if !line.physical {
        return no("powerinput", String::from("the request does not say that real input asked for it (\"input\":\"physical\")"));
    }
    Ok(String::from("holds the compositor grant, input physical as the compositor says"))
}

/// The request line out of what a client sent (at most `power::REQUEST_MAX` bytes, read until a newline): the
/// text before the first newline; None when it is empty. A line cut at the size limit stays cut (and then is
/// not understood).
pub fn first_line(buf: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(&buf[..buf.len().min(crate::power::REQUEST_MAX)]);
    let first = text.split('\n').next().unwrap_or("");
    (!first.trim().is_empty()).then(|| first.to_string())
}

/// The answer line: UL-7's plain `ok <op>` / `status <line>` / `refused <why>`, or as JSON for a JSON request
/// (`{"ok":true,"op":"poweroff"}`, `{"ok":true,"status":"..."}`, `{"ok":false,"refused":"..."}`).
pub enum Answer<'a> {
    Ok(Action),
    Status(&'a str),
    Refused(&'a str),
}

pub fn answer(json: bool, a: &Answer) -> String {
    let op = |a: &Action| if *a == Action::Restart { "restart" } else { "poweroff" };
    match (json, a) {
        (false, Answer::Ok(x)) => format!("ok {}\n", op(x)),
        (false, Answer::Status(s)) => format!("status {s}\n"),
        (false, Answer::Refused(w)) => format!("refused {w}\n"),
        (true, Answer::Ok(x)) => format!("{{\"ok\":true,\"op\":\"{}\"}}\n", op(x)),
        (true, Answer::Status(s)) => format!("{{\"ok\":true,\"status\":{}}}\n", quote(s)),
        (true, Answer::Refused(w)) => format!("{{\"ok\":false,\"refused\":{}}}\n", quote(w)),
    }
}

/// A JSON string literal of `s`.
pub fn quote(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// One JSON object, no key twice: its keys with the string values (Some) and the other values (None: numbers,
/// true/false/null, nested objects and arrays, checked for their form and skipped, so a client may add fields).
/// None for anything else (never a panic on input).
fn object(t: &str) -> Option<Vec<(String, Option<String>)>> {
    let mut p = Parser { s: t.as_bytes(), i: 0 };
    p.eat(b'{')?;
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    if p.peek() == Some(b'}') {
        p.i += 1;
    } else {
        loop {
            let k = p.string()?;
            p.eat(b':')?;
            let v = if p.peek() == Some(b'"') { Some(p.string()?) } else { p.skip_value()?; None };
            if out.iter().any(|(n, _)| *n == k) {
                return None;
            }
            out.push((k, v));
            match p.next()? {
                b',' => continue,
                b'}' => break,
                _ => return None,
            }
        }
    }
    p.skip_ws();
    (p.i == p.s.len()).then_some(out)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while self.s.get(self.i).is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n')) {
            self.i += 1;
        }
    }
    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.s.get(self.i).copied()
    }
    fn next(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.i += 1;
        Some(b)
    }
    fn eat(&mut self, want: u8) -> Option<()> {
        (self.next()? == want).then_some(())
    }
    /// A JSON value other than a string at the top: a number, true/false/null, or an object or array (its
    /// nesting counted, not recursed into, so no input can deepen the stack).
    fn skip_value(&mut self) -> Option<()> {
        let mut open: Vec<u8> = Vec::new();
        loop {
            match self.peek()? {
                b'{' | b'[' => {
                    open.push(self.s[self.i]);
                    self.i += 1;
                    if matches!(self.peek(), Some(b'}' | b']')) {
                        self.close(&mut open)?;
                    } else {
                        if open.last() == Some(&b'{') {
                            self.string()?;
                            self.eat(b':')?;
                        }
                        continue;
                    }
                }
                b'"' => {
                    self.string()?;
                }
                b'-' | b'0'..=b'9' => self.number()?,
                b't' | b'f' | b'n' => {
                    let rest = &self.s[self.i..];
                    let w = [&b"true"[..], b"false", b"null"].into_iter().find(|w| rest.starts_with(w))?;
                    self.i += w.len();
                }
                _ => return None,
            }
            // After a value: the next member or element, or the end of the containers it closes.
            loop {
                if open.is_empty() {
                    return Some(());
                }
                match self.peek()? {
                    b',' => {
                        self.i += 1;
                        if open.last() == Some(&b'{') {
                            self.string()?;
                            self.eat(b':')?;
                        }
                        break;
                    }
                    b'}' | b']' => self.close(&mut open)?,
                    _ => return None,
                }
            }
        }
    }
    fn close(&mut self, open: &mut Vec<u8>) -> Option<()> {
        let want = if open.pop()? == b'{' { b'}' } else { b']' };
        self.eat(want)
    }
    fn number(&mut self) -> Option<()> {
        let start = self.i;
        while self.s.get(self.i).is_some_and(|b| matches!(b, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')) {
            self.i += 1;
        }
        std::str::from_utf8(&self.s[start..self.i]).ok()?.parse::<f64>().ok().map(|_| ())
    }
    fn string(&mut self) -> Option<String> {
        self.eat(b'"')?;
        let mut out = Vec::new();
        loop {
            let b = *self.s.get(self.i)?;
            self.i += 1;
            match b {
                b'"' => return String::from_utf8(out).ok(),
                b'\\' => {
                    let e = *self.s.get(self.i)?;
                    self.i += 1;
                    let c = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let h = std::str::from_utf8(self.s.get(self.i..self.i + 4)?).ok()?;
                            self.i += 4;
                            char::from_u32(u32::from_str_radix(h, 16).ok()?).unwrap_or('\u{fffd}')
                        }
                        _ => return None,
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
                b if b < 0x20 => return None,
                b => out.push(b),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(t: &str) -> Line {
        parse_line(t).unwrap()
    }

    #[test]
    fn json_and_plain_lines() {
        let l = line("{\"op\":\"poweroff\",\"input\":\"physical\"}\n");
        assert_eq!(l, Line { request: Request::Do(Action::PowerOff), physical: true, json: true });
        assert_eq!(line(" { \"input\" : \"physical\" , \"op\" : \"reboot\" } ").request, Request::Do(Action::Restart));
        assert!(!line("{\"op\":\"reboot\"}").physical);
        assert!(!line("{\"op\":\"reboot\",\"input\":\"injected\"}").physical);
        assert_eq!(line("{\"op\":\"status\"}").request, Request::Status);
        assert_eq!(line("{\"op\":\"po\\u0077eroff\",\"x\":\"\\\"\"}").request, Request::Do(Action::PowerOff));
        let extra = line("{\"seq\":1,\"op\":\"poweroff\",\"confirmed\":true,\"x\":null,\"t\":-1.5e3,\"a\":[1,[\"b\"],{}],\"o\":{\"k\":{\"z\":[]}},\"input\":\"physical\"}");
        assert_eq!(extra, Line { request: Request::Do(Action::PowerOff), physical: true, json: true });
        assert!(!line("{\"op\":\"poweroff\",\"input\":true}").physical);
        assert!(parse_line("{\"op\":true}").is_err_and(|e| e.contains("not a string")));
        assert_eq!(line("poweroff"), Line { request: Request::Do(Action::PowerOff), physical: false, json: false });
        assert_eq!(line("status\n").request, Request::Status);
        for bad in [
            "",
            "{",
            "{}",
            "{\"op\":\"shutdown\"}",
            "{\"op\":\"reboot\",\"op\":\"poweroff\"}",
            "{\"op\":1}",
            "{\"op\":\"reboot\",\"n\":01x}",
            "{\"op\":\"reboot\",\"n\":tru}",
            "{\"op\":\"reboot\",\"a\":[1,}",
            "{\"op\":\"reboot\",\"a\":[1}]}",
            "{\"op\":\"reboot\",\"o\":{1:2}}",
            "{\"op\":\"reboot\",\"o\":{\"k\":}}",
            "{\"op\":\"reboot\"} x",
            "{\"op\":\"reboot\",}",
            "{\"op\":\"reb\\qoot\"}",
            "{\"op\":\"reb\\u00\"}",
            "{\"op\":\"\u{1}\"}",
            "{\"op\" \"reboot\"}",
            "[\"reboot\"]",
            "shutdown -h now",
        ] {
            assert!(parse_line(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn never_panics_on_cut_lines() {
        let full = "{\"op\":\"poweroff\",\"input\":\"physical\",\"w\":\"\\u00e4\\n\"}";
        for n in 0..full.len() {
            let _ = parse_line(&full[..n]);
        }
        for b in 0u8..=255 {
            let _ = parse_line(&String::from_utf8_lossy(&[b'{', b'"', b]));
        }
        let nested = "{\"op\":\"reboot\",\"a\":[{\"b\":[1,true,{\"c\":\"]\"}]}],\"n\":-0.5}";
        assert!(parse_line(nested).is_ok());
        for n in 0..nested.len() {
            let _ = parse_line(&nested[..n]);
        }
        // Deep nesting within the line limit: no recursion, so no stack overflow.
        let deep = format!("{{\"op\":\"reboot\",\"a\":{}{}}}", "[".repeat(5000), "]".repeat(5000));
        assert!(parse_line(&deep).is_ok());
    }

    #[test]
    fn holder_and_input_rule() {
        let phys = line("{\"op\":\"poweroff\",\"input\":\"physical\"}");
        let plain = line("poweroff");
        assert!(do_allowed(412, Some("holder=412\n"), &phys).is_ok());
        let no = |pid, h: Option<&str>, l: &Line| do_allowed(pid, h, l).unwrap_err();
        let r = no(412, Some("holder=412\n"), &plain);
        assert!(r.kind == "powerinput" && r.why.contains("real input"));
        let r = no(7, Some("holder=412\n"), &phys);
        assert!(r.kind == "powerholder" && r.why.contains("pid 412 does"));
        assert!(no(7, Some("holder=none\n"), &phys).why.contains("no process holds"));
        assert!(no(7, None, &phys).why.contains("cannot be read"));
        assert!(no(7, Some("holder=0"), &phys).why.contains("not understood"));
        assert_eq!(holder_of("holder=12 13"), None);
        assert_eq!(holder_of("holder=none"), Some(None));
    }

    #[test]
    fn line_cut() {
        assert_eq!(first_line(b"{\"op\":\"reboot\"}\nrest"), Some(String::from("{\"op\":\"reboot\"}")));
        assert_eq!(first_line(b""), None);
        assert_eq!(first_line(b"  \nstatus\n"), None);
        assert_eq!(first_line(b"status"), Some(String::from("status")));
        let long = vec![b'x'; crate::power::REQUEST_MAX * 3];
        assert_eq!(first_line(&long).map(|l| l.len()), Some(crate::power::REQUEST_MAX));
        assert!(first_line(&[0xff, b'\n']).is_some());
        // A request cut at the limit is not understood (refused), never half taken.
        let cut = format!("{{\"op\":\"poweroff\",\"pad\":\"{}\"}}", "y".repeat(crate::power::REQUEST_MAX));
        assert!(parse_line(&first_line(cut.as_bytes()).unwrap()).is_err());
    }

    #[test]
    fn answers() {
        assert_eq!(answer(false, &Answer::Ok(Action::PowerOff)), "ok poweroff\n");
        assert_eq!(answer(false, &Answer::Refused("x")), "refused x\n");
        assert_eq!(answer(true, &Answer::Ok(Action::Restart)), "{\"ok\":true,\"op\":\"restart\"}\n");
        assert_eq!(answer(true, &Answer::Status("poweroff=allowed age_ms=3")), "{\"ok\":true,\"status\":\"poweroff=allowed age_ms=3\"}\n");
        assert_eq!(answer(true, &Answer::Refused("a \"b\"\n")), "{\"ok\":false,\"refused\":\"a \\\"b\\\"\\n\"}\n");
        let j = answer(true, &Answer::Refused("\u{1}"));
        assert!(j.contains("\\u0001"));
    }
}
