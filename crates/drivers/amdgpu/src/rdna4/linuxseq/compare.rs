//! GX-10: a stage's accesses against a section of Linux's trace, strictly in order. Every difference counts: a
//! write or read Linux does not make, one it makes and the stage does not, another order, another value, a wait on
//! another register, for another condition or with another bound, a sleep Linux does not make or of another length,
//! a write that did not reach the card, an answer of the card (in a test the fake one) that is not what the real
//! card answered Linux. How many reads a wait took and how long is not compared (the card's doing), nor how many
//! rounds a declared pair poll took (`rules::PairPoll`). The answer is
//! `Ok` or the first difference in plain words: the fixture's line, what Linux did, what the stage did.
//!
//! One `Compare` runs over a section with a cursor, so several stages can cover one section in turn (`feed` per
//! stage, then `finish`); `stop` ends it early, after a prefix.
//!
//! A stage's own answer (`chainseq::verdict`, on the real card as on the fake) asks with `answers_kept`: an
//! answer that differs from the trace's is the card's doing then, kept and counted, and the accesses after it are
//! compared on. A test of the fake card keeps the default, where such an answer is the first difference.

use super::fold::{Bus, Did, fold};
use super::rules::Rule;
use super::{Line, Section, What, words};
use crate::gpucore::psp::Bound;
use crate::gpucore::trail::Access;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

pub use super::words::render;

pub use super::diff::{Diff, Kind};

pub struct Compare<'a> {
    sec: &'a Section<'a>,
    rules: &'a [Rule],
    hdp: Option<(u32, u32)>,
    next: usize,
    /// Reads already matched of a read line that stands for several.
    rep: u32,
    /// Accesses already matched of what the rules put before the line at `next` (`before`).
    pre: usize,
    /// Linux's TLB flush after a GART bind, when a rule asks for it: MMHUB's alone, and both hubs'.
    bind: Vec<Line<'static>>,
    bind_both: Vec<Line<'static>>,
    bind_mes: Vec<Line<'static>>,
    /// Per rule: the line its bind flush is compared before (a bind comes once, before the first write).
    bind_at: Vec<Option<usize>>,
    used: Vec<u32>,
    /// Per address rule: the low half (stage's, Linux's) and the high half (stage's) seen so far.
    half: Vec<(Option<(u32, u32)>, Option<u32>)>,
    /// Per address rule: the address its registers named when the rule last held.
    held_at: Vec<Option<u64>>,
    /// `answers_kept`: the answers that differ from the trace's, in order, instead of ending the comparison.
    answers: Option<Vec<Diff>>,
}

/// What a rule puts right before one of Linux's writes.
#[derive(Clone, Copy)]
enum Pre {
    Hdp,
    /// A line of Linux's flush after a bind, and the fixture it is cut from.
    Bind(Line<'static>, &'static str),
    /// A read of this dword (`Rule::Untraced`).
    Read(u32),
}

impl<'a> Compare<'a> {
    /// Every rule must name a register this section writes, and a register has at most one rule of a kind.
    pub fn new(sec: &'a Section<'a>, rules: &'a [Rule]) -> Result<Compare<'a>, Diff> {
        let n = rules.len();
        let mut c = Compare { sec, rules, hdp: super::hdp_pair(), next: 0, rep: 0, pre: 0, bind: Vec::new(), bind_both: Vec::new(), bind_mes: Vec::new(), bind_at: vec![None; n], used: vec![0; n], half: vec![(None, None); n], held_at: vec![None; n], answers: None };
        if rules.iter().any(|r| matches!(r, Rule::BindBefore { .. })) {
            c.bind = super::bind_flush().map_err(|e| c.rule_diff(e))?;
        }
        if rules.iter().any(|r| matches!(r, Rule::BindBothBefore { .. })) {
            c.bind_both = super::bind_flush_both().map_err(|e| c.rule_diff(e))?;
        }
        if rules.iter().any(|r| matches!(r, Rule::BindMesBefore { .. })) {
            c.bind_mes = super::bind_flush_mes().map_err(|e| c.rule_diff(e))?;
        }
        for (i, r) in rules.iter().enumerate() {
            if let Rule::PairPoll(p) = r
                && !sec.lines.windows(2).any(|w| matches!((w[0].what, w[1].what), (What::Read { name: a, times: 1, .. }, What::Read { name: b, times: 1, .. }) if a == p.first && b == p.second))
            {
                return Err(c.rule_diff(format!("a rule names the pair {} and {}, which Linux never reads one after the other in this section ({})", p.first, p.second, p.why)));
            }
            for n in r.names() {
                if !matches!(r, Rule::PairPoll(_)) && !sec.lines.iter().any(|l| matches!(l.what, What::Write { name, .. } if name == n)) {
                    return Err(c.rule_diff(format!("a rule names {n}, which Linux never writes in this section ({})", r.why())));
                }
                if rules[..i].iter().any(|o| o.clashes(r)) {
                    return Err(c.rule_diff(format!("two rules of one kind name {n}")));
                }
            }
        }
        Ok(c)
    }

    /// From here an answer of the card that differs from the trace's does not end the comparison: it is kept
    /// (`answers`) and the next access is compared. What the stage itself does is held as strictly as before.
    pub fn answers_kept(mut self) -> Compare<'a> {
        self.answers = Some(Vec::new());
        self
    }

    /// The answers kept so far (none without `answers_kept`).
    pub fn answers(&self) -> &[Diff] {
        self.answers.as_deref().unwrap_or(&[])
    }

    /// Per rule, in the rules' order: the address the stage named under an address rule that held (`None`: no
    /// address rule, or not compared yet). What one section's stage was told can be asked of another's with it.
    pub fn addresses(&self) -> &[Option<u64>] {
        &self.held_at
    }

    /// The line of init.txt the next compared access is held against (`None`: the section is used up).
    pub fn next_line(&self) -> Option<u32> {
        self.sec.lines.get(self.next).map(|l| l.init)
    }

    fn rule_diff(&self, text: String) -> Diff {
        Diff { kind: Kind::Rule, section: String::from(self.sec.name), at: None, text }
    }

    fn diff(&self, kind: Kind, l: &Line, text: String) -> Diff {
        Diff { kind, section: String::from(self.sec.name), at: Some((l.at, l.init)), text }
    }

    fn linux(&self, w: &What) -> String {
        words::linux(w)
    }

    fn did(&self, d: &Did) -> String {
        words::did(self.sec, d)
    }

    /// A stage's trail (`lost`: what the owner's trail dropped), folded as Linux's trace would show it.
    pub fn feed_trail(&mut self, trail: &[Access], lost: u64, bus: &Bus) -> Result<(), Diff> {
        if lost != 0 {
            return Err(Diff { kind: Kind::Lost, section: String::from(self.sec.name), at: None, text: format!("the stage's trail lost {lost} accesses: it proves no sequence") });
        }
        let did = fold(trail, bus).map_err(|e| Diff { kind: Kind::Lost, section: String::from(self.sec.name), at: None, text: format!("the stage's trail holds {e}") })?;
        self.feed(&did)
    }

    pub fn feed(&mut self, did: &[Did]) -> Result<(), Diff> {
        let mut i = 0;
        while i < did.len() {
            let d = &did[i];
            let Some(l) = self.sec.lines.get(self.next) else {
                let last = self.sec.lines.last().ok_or_else(|| self.rule_diff(String::from("an empty section")))?;
                return Err(self.diff(Kind::Extra, last, format!("Linux's section ends after this line; the stage goes on and {}", self.did(d))));
            };
            // The rounds of a declared pair poll before its last one take no line of the fixture either.
            if self.rep == 0 && self.pre == 0 {
                let rounds = self.poll_rounds(&did[i..])?;
                if rounds != 0 {
                    i += rounds;
                    continue;
                }
            }
            // What a rule declares comes right before Linux's write and takes no line of the fixture.
            let pre = self.before();
            if let Some(&(r, p)) = pre.get(self.pre) {
                self.pre_one(l, r, p, d)?;
                self.pre += 1;
                self.bind_at[r] = Some(self.next);
                if pre.get(self.pre).is_none_or(|n| n.0 != r) {
                    self.used[r] += 1;
                }
                i += 1;
                continue;
            }
            self.one(l, d, self.sec.lines.get(self.next + 1), did.get(i + 1))?;
            let times = if let What::Read { times, .. } = l.what { times } else { 1 };
            self.rep += 1;
            if self.rep >= times {
                (self.next, self.rep, self.pre) = (self.next + 1, 0, 0);
            }
            i += 1;
        }
        Ok(())
    }

    /// With the cursor at the two reads of a declared pair poll (`rules::PairPoll`): how many of the accesses at
    /// the head of `did` are rounds before the stage's last one.
    fn poll_rounds(&mut self, did: &[Did]) -> Result<usize, Diff> {
        let (Some(a), Some(b)) = (self.sec.lines.get(self.next), self.sec.lines.get(self.next + 1)) else { return Ok(0) };
        let (What::Read { name: first, dword: d0, .. }, What::Read { name: second, dword: d1, .. }) = (a.what, b.what) else { return Ok(0) };
        let rules = self.rules;
        let found = rules.iter().enumerate().find_map(|(i, r)| match r {
            Rule::PairPoll(p) if p.first == first && p.second == second => Some((i, p)),
            _ => None,
        });
        let Some((i, p)) = found else { return Ok(0) };
        self.used[i] += 1;
        p.rounds(d0, d1, did).map_err(|(kind, text)| self.diff(kind, a, text))
    }

    /// What the rules put before the line at `next`, in the rules' order: (rule, one access).
    fn before(&self) -> Vec<(usize, Pre)> {
        let Some(What::Write { name, .. }) = self.sec.lines.get(self.next).map(|l| l.what) else { return Vec::new() };
        let mut out = Vec::new();
        for (r, rule) in self.rules.iter().enumerate() {
            match rule {
                Rule::BindBefore { reg, .. } if *reg == name && self.bind_at[r].is_none_or(|at| at == self.next) => out.extend(self.bind.iter().map(|b| (r, Pre::Bind(*b, "sw-init")))),
                Rule::BindBothBefore { reg, .. } if *reg == name && self.bind_at[r].is_none_or(|at| at == self.next) => {
                    // GFXHUB's three lines are `gc-hub`'s last, between the first and the second of `sw-init`'s.
                    out.extend(self.bind_both.iter().enumerate().map(|(i, b)| (r, Pre::Bind(*b, if (1..=3).contains(&i) { "gc-hub" } else { "sw-init" }))))
                }
                Rule::BindMesBefore { reg, .. } if *reg == name && self.bind_at[r].is_none_or(|at| at == self.next) => out.extend(self.bind_mes.iter().map(|b| (r, Pre::Bind(*b, "mes")))),
                Rule::HdpBefore { reg, .. } if *reg == name => out.push((r, Pre::Hdp)),
                Rule::Untraced { first, byte, .. } if *first == name && self.next == 0 => out.push((r, Pre::Read(byte / 4))),
                _ => {}
            }
        }
        out
    }

    fn pre_one(&mut self, l: &Line, r: usize, p: Pre, d: &Did) -> Result<(), Diff> {
        let (reg, why) = (self.rules[r].names().next().unwrap_or(""), self.rules[r].why());
        match p {
            Pre::Hdp => match (*d, self.hdp) {
                (Did::Hdp { flush, mem, .. }, Some(pair)) if (flush, mem) == pair => Ok(()),
                _ => Err(self.diff(Kind::Missing, l, format!("BitsOS flushes the HDP as Linux does right before the write of {reg} ({why}); the stage {} instead", self.did(d)))),
            },
            Pre::Read(dword) => match *d {
                Did::Read { dword: at, .. } if at == dword => Ok(()),
                _ => Err(self.diff(Kind::Missing, l, format!("Linux reads the register at byte {:#x} through the index pair right before this line, where its trace shows nothing ({why}); the stage {} instead", u64::from(dword) * 4, self.did(d)))),
            },
            // Each line of Linux's flush is held like a line of the section; another access altogether means the
            // flush is not there.
            Pre::Bind(b, from) => self.one(&b, d, None, None).map_err(|e| {
                let kind = if e.kind == Kind::Other { Kind::Missing } else { e.kind };
                let text = format!("BitsOS binds GART pages before the first write of {reg} and flushes the TLB as Linux does after a bind ({why}; {from}.txt line {}): {}", b.at, e.text);
                self.diff(kind, l, text)
            }),
        }
    }

    fn one(&mut self, l: &Line, d: &Did, after_l: Option<&Line>, after_d: Option<&Did>) -> Result<(), Diff> {
        match (self.held(l, d, after_l, after_d), self.answers.as_mut()) {
            (Err(e), Some(kept)) if e.kind == Kind::Answer => {
                kept.push(e);
                Ok(())
            }
            (r, _) => r,
        }
    }

    fn held(&mut self, l: &Line, d: &Did, after_l: Option<&Line>, after_d: Option<&Did>) -> Result<(), Diff> {
        if !same_place(&l.what, d) {
            let skips_linux = after_l.is_some_and(|n| same_place(&n.what, d));
            let skips_stage = after_d.is_some_and(|n| same_place(&l.what, n));
            let (kind, text) = match (skips_linux, skips_stage, after_l) {
                (true, true, Some(n)) => (Kind::Order, format!("Linux {} and then {}; the stage does the two the other way round", self.linux(&l.what), self.linux(&n.what))),
                (true, false, _) => (Kind::Missing, format!("Linux {}; the stage leaves that out and {}", self.linux(&l.what), self.did(d))),
                (false, true, _) => (Kind::Extra, format!("Linux {}; the stage first {}, which Linux does not do here", self.linux(&l.what), self.did(d))),
                _ => (Kind::Other, format!("Linux {}; the stage {}", self.linux(&l.what), self.did(d))),
            };
            return Err(self.diff(kind, l, text));
        }
        match (l.what, *d) {
            (What::Write { .. }, Did::Write { reached: false, .. }) => Err(self.diff(Kind::Refused, l, format!("Linux {}; the stage {}", self.linux(&l.what), self.did(d)))),
            (What::Write { name, value: linux, .. }, Did::Write { value, .. }) => self.value(l, name, linux, value),
            (What::Read { value: linux, .. }, Did::Read { value, .. }) | (What::Hdp { value: linux, .. }, Did::Hdp { value, .. }) if value != Some(linux) => {
                Err(self.diff(Kind::Answer, l, format!("Linux {}; the stage {}: the card does not answer as it answered Linux", self.linux(&l.what), self.did(d))))
            }
            (What::Wait { cond: linux, .. }, Did::Wait { cond: c, .. }) if linux != c => {
                Err(self.diff(Kind::Condition, l, format!("Linux {}; the stage {}: another condition", self.linux(&l.what), self.did(d))))
            }
            (What::Wait { max, .. }, Did::Wait { bound, .. }) if bound != Bound::Reads(max) => {
                Err(self.diff(Kind::Bound, l, format!("Linux {} and gives up after {}; the stage gives up after {bound}", self.linux(&l.what), Bound::Reads(max))))
            }
            (What::Sleep { us: linux, .. }, Did::Sleep { us }) if us != linux => Err(self.diff(Kind::Value, l, format!("Linux {}; the stage {}", self.linux(&l.what), self.did(d)))),
            (What::Wait { last: linux, .. }, Did::Wait { reads, last, .. }) if reads == 0 || last != Some(linux) => {
                Err(self.diff(Kind::Answer, l, format!("Linux {}; the stage {}: the card does not answer as it answered Linux", self.linux(&l.what), self.did(d))))
            }
            _ => Ok(()),
        }
    }

    /// The message a write of a mailbox's argument register is for: the value of the next write of `msg` in
    /// Linux's section.
    fn message_after(&self, l: &Line, msg: &str) -> Option<u32> {
        let at = self.sec.lines.iter().position(|x| x == l)?;
        self.sec.lines[at + 1..].iter().find_map(|x| match x.what {
            What::Write { name, value, .. } if name == msg => Some(value),
            _ => None,
        })
    }

    /// A write of an argument register that a rule declares as half of an address: checked when both halves are
    /// there. `None`: no rule speaks for this write.
    fn argument(&mut self, l: &Line, name: &str, linux: u32, value: u32) -> Option<Result<(), Diff>> {
        let rules = self.rules;
        let (i, a, high) = rules.iter().enumerate().find_map(|(i, r)| match r {
            Rule::Argument(a) if a.arg == name => match self.message_after(l, a.msg) {
                Some(m) if m == a.hi => Some((i, a, true)),
                Some(m) if m == a.lo => Some((i, a, false)),
                _ => None,
            },
            _ => None,
        })?;
        self.used[i] += 1;
        let (lo, hi) = &mut self.half[i];
        let again = if high { hi.replace(value).is_some() } else { lo.replace((value, linux)).is_some() };
        if again {
            return Some(Err(self.diff(Kind::Rule, l, format!("{name} carries the same half of an address a second time ({})", a.why))));
        }
        let (Some((stage_lo, _)), Some(stage_hi)) = (*lo, *hi) else { return Some(Ok(())) };
        self.half[i] = (None, None);
        Some(a.check(stage_hi, stage_lo).map(|_| ()).map_err(|e| self.diff(Kind::Value, l, format!("Linux writes {linux:#010x} to {name}; the stage writes {value:#010x}, and the rule for it ({}) does not hold: {e}", a.why))))
    }

    /// A write of the same register: Linux's value, or an address a rule declares and checks.
    fn value(&mut self, l: &Line, name: &str, linux: u32, value: u32) -> Result<(), Diff> {
        if let Some(r) = self.argument(l, name, linux, value) {
            return r;
        }
        let rules = self.rules;
        let found = rules.iter().enumerate().find_map(|(i, r)| match r {
            Rule::Address(a) if a.lo == name || a.hi == Some(name) => Some((i, a)),
            _ => None,
        });
        let Some((i, a)) = found else {
            if value == linux {
                return Ok(());
            }
            return Err(self.diff(Kind::Value, l, format!("Linux writes {linux:#010x} to {name}; the stage writes {value:#010x}, and no rule declares a difference for {name}")));
        };
        self.used[i] += 1;
        let (lo, hi) = &mut self.half[i];
        let again = if a.lo == name { lo.replace((value, linux)).is_some() } else { hi.replace(value).is_some() };
        if again {
            return Err(self.diff(Kind::Rule, l, format!("{name} is written a second time before the other half of its address ({})", a.why)));
        }
        let (stage_lo, linux_lo, stage_hi) = match (*lo, *hi, a.hi) {
            (Some((s, x)), Some(h), _) => (s, x, h),
            (Some((s, x)), None, None) => (s, x, 0),
            _ => return Ok(()),
        };
        self.half[i] = (None, None);
        match a.check(stage_lo, stage_hi, linux_lo) {
            Ok(addr) => {
                self.held_at[i] = Some(addr);
                Ok(())
            }
            Err(e) => Err(self.diff(Kind::Value, l, format!("Linux writes {linux:#010x} to {name}; the stage writes {value:#010x}, and the rule for it ({}) does not hold: {e}", a.why))),
        }
    }

    /// Ends the comparison where it stands (a prefix of the section): every rule must have been used and no
    /// address may be half compared. The line of init.txt that would come next.
    pub fn stop(self) -> Result<Option<u32>, Diff> {
        if self.rep != 0 {
            let l = &self.sec.lines[self.next];
            return Err(self.diff(Kind::Missing, l, format!("Linux {} more often than the stage", self.linux(&l.what))));
        }
        if self.pre != 0 {
            let l = &self.sec.lines[self.next];
            return Err(self.diff(Kind::Missing, l, format!("Linux {}; the stage ends before that, after what a rule puts before it", self.linux(&l.what))));
        }
        for (i, r) in self.rules.iter().enumerate() {
            let names: Vec<&str> = r.names().collect();
            if self.used[i] == 0 {
                return Err(self.rule_diff(format!("the rule for {} was used by no compared line ({}): a rule nothing needs is an error", names.join(" and "), r.why())));
            }
            if self.half[i] != (None, None) {
                return Err(self.rule_diff(format!("only one half of the address in {} was compared ({})", names.join(" and "), r.why())));
            }
        }
        Ok(self.next_line())
    }

    /// Ends the comparison at the section's end: as `stop`, and Linux must have nothing left.
    pub fn finish(self) -> Result<(), Diff> {
        if let Some(l) = self.sec.lines.get(self.next)
            && self.rep == 0
        {
            return Err(self.diff(Kind::Missing, l, format!("Linux goes on and {}; the stage did nothing more", self.linux(&l.what))));
        }
        self.stop().map(|_| ())
    }
}

/// The same kind of access to the same register (values and conditions aside).
fn same_place(w: &What, d: &Did) -> bool {
    match (*w, *d) {
        (What::Write { dword: a, .. }, Did::Write { dword: b, .. }) | (What::Read { dword: a, .. }, Did::Read { dword: b, .. }) | (What::Wait { dword: a, .. }, Did::Wait { dword: b, .. }) => a == b,
        (What::Hdp { flush: a, mem: m, .. }, Did::Hdp { flush: b, mem: n, .. }) => a == b && m == n,
        (What::Sleep { .. }, Did::Sleep { .. }) => true,
        _ => false,
    }
}
