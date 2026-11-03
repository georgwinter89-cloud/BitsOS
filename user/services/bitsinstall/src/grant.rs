//! Standing grants: set at the physical keyboard (bitscomp, MC-9b) for one kind of action until a wall-clock
//! time ("Claude darf bis 8 Uhr installieren"); while one holds, requests of that kind pass without a dialog.
//! A grant ends by itself: at its time, when the clock goes back before the moment it was set (the clock is not
//! trusted to only move forward, so a jump back ends it rather than stretching it), and it can never be longer
//! than `MAX_S`. The NVMe gate and unknown actions are never granted (`Kind::grantable`). Time is Unix seconds.

use mcp_wire::installer::Kind;

/// The longest grant: one day.
pub const MAX_S: i64 = 24 * 3600;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grant {
    pub kind: Kind,
    pub until: i64,
    pub set_at: i64,
}

/// Why a grant ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    Expired,
    ClockBack,
    Revoked,
    Replaced,
}

impl Ended {
    pub fn text(self) -> &'static str {
        match self {
            Ended::Expired => "expired",
            Ended::ClockBack => "ended: the clock went back before it was set",
            Ended::Revoked => "revoked at the keyboard",
            Ended::Replaced => "replaced by a new grant",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Grants {
    list: Vec<Grant>,
}

impl Grants {
    pub fn all(&self) -> &[Grant] {
        &self.list
    }

    /// A grant for `kind` until `until`, set at `now`; the older one of that kind is replaced (returned).
    pub fn set(&mut self, kind: Kind, until: i64, now: i64) -> Result<(Grant, Option<Grant>), String> {
        if !kind.grantable() {
            return Err(format!("{} is never granted: it is always asked", kind.name()));
        }
        if until <= now {
            return Err(format!("the time {until} has passed (now {now})"));
        }
        if until.checked_sub(now).is_none_or(|d| d > MAX_S) {
            return Err(format!("a grant lasts at most {} h", MAX_S / 3600));
        }
        let g = Grant { kind, until, set_at: now };
        let old = self.list.iter().position(|o| o.kind == kind).map(|i| self.list.remove(i));
        self.list.push(g);
        self.list.sort_by_key(|g| g.kind);
        Ok((g, old))
    }

    /// Ends the grant of `kind` (None: every grant).
    pub fn revoke(&mut self, kind: Option<Kind>) -> Vec<Grant> {
        let (gone, kept) = self.list.iter().partition(|g| kind.is_none_or(|k| g.kind == k));
        self.list = kept;
        gone
    }

    /// Removes what ended by `now` and says why.
    pub fn sweep(&mut self, now: i64) -> Vec<(Grant, Ended)> {
        let mut ended = Vec::new();
        self.list.retain(|g| {
            let why = if now < g.set_at {
                Some(Ended::ClockBack)
            } else if now >= g.until || g.until - g.set_at > MAX_S {
                Some(Ended::Expired)
            } else {
                None
            };
            if let Some(w) = why {
                ended.push((*g, w));
            }
            why.is_none()
        });
        ended
    }

    /// The grant that lets `kind` through at `now` (call `sweep` first to journal what ended).
    pub fn covering(&self, kind: Kind, now: i64) -> Option<Grant> {
        self.list.iter().copied().find(|g| g.kind == kind && kind.grantable() && g.set_at <= now && now < g.until && g.until - g.set_at <= MAX_S)
    }

    /// The file's form: one line `kind until set_at` per grant.
    pub fn to_text(&self) -> String {
        self.list.iter().map(|g| format!("{} {} {}\n", g.kind.name(), g.until, g.set_at)).collect()
    }

    /// Reads the file's form; lines that do not parse are skipped and counted.
    pub fn from_text(text: &str) -> (Grants, usize) {
        let mut g = Grants::default();
        let mut bad = 0;
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let w: Vec<&str> = line.split_whitespace().collect();
            let parsed = match w[..] {
                [k, u, s] => Kind::parse(k).filter(|k| k.grantable()).zip(u.parse::<i64>().ok()).zip(s.parse::<i64>().ok()),
                _ => None,
            };
            match parsed {
                Some(((kind, until), set_at)) if until > set_at && until.checked_sub(set_at).is_some_and(|d| d <= MAX_S) && !g.list.iter().any(|o| o.kind == kind) => {
                    g.list.push(Grant { kind, until, set_at })
                }
                _ => bad += 1,
            }
        }
        g.list.sort_by_key(|g| g.kind);
        (g, bad)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: i64 = 1_800_000_000;

    #[test]
    fn a_grant_lets_its_kind_through_until_its_time() {
        let mut g = Grants::default();
        let (grant, old) = g.set(Kind::Install, T + 3600, T).unwrap();
        assert_eq!(old, None);
        assert_eq!(g.covering(Kind::Install, T + 10), Some(grant));
        assert_eq!(g.covering(Kind::Reload, T + 10), None, "kinds are separate");
        assert_eq!(g.covering(Kind::Install, T + 3599), Some(grant));
        assert_eq!(g.covering(Kind::Install, T + 3600), None, "ends at its time");
        assert!(g.sweep(T + 3599).is_empty());
        assert_eq!(g.sweep(T + 3600), vec![(grant, Ended::Expired)]);
        assert!(g.all().is_empty());
    }

    #[test]
    fn what_is_never_granted_and_bad_times() {
        let mut g = Grants::default();
        assert!(g.set(Kind::Nvme, T + 60, T).unwrap_err().contains("never granted"));
        assert!(g.set(Kind::Other, T + 60, T).is_err());
        assert!(g.set(Kind::Install, T, T).unwrap_err().contains("passed"));
        assert!(g.set(Kind::Install, T + MAX_S + 1, T).unwrap_err().contains("at most 24 h"));
        assert!(g.set(Kind::Install, T + MAX_S, T).is_ok());
        assert!(g.set(Kind::Pin, i64::MAX, -T).is_err(), "no overflow from a far time and an odd clock");
        assert_eq!(g.covering(Kind::Nvme, T), None);
    }

    #[test]
    fn the_clock_going_back_ends_a_grant() {
        let mut g = Grants::default();
        let (grant, _) = g.set(Kind::Reload, T + 600, T).unwrap();
        assert_eq!(g.covering(Kind::Reload, T - 1), None);
        assert_eq!(g.sweep(T - 1), vec![(grant, Ended::ClockBack)]);
        assert_eq!(g.covering(Kind::Reload, T + 1), None, "gone for good");
    }

    #[test]
    fn replace_revoke_and_the_file() {
        let mut g = Grants::default();
        let (a, _) = g.set(Kind::Install, T + 600, T).unwrap();
        g.set(Kind::Reload, T + 900, T).unwrap();
        let (b, old) = g.set(Kind::Install, T + 1200, T + 5).unwrap();
        assert_eq!(old, Some(a));
        assert_eq!(g.covering(Kind::Install, T + 1000), Some(b));
        let text = g.to_text();
        assert_eq!(text, format!("install {} {}\nreload {} {}\n", T + 1200, T + 5, T + 900, T));
        let (back, bad) = Grants::from_text(&(text.clone() + "nvme 1 0\nall 5 1\ninstall 9 1\njunk\nreboot 10 5 extra\n"));
        assert_eq!(bad, 5, "nvme, an unknown kind, a second install, junk, extra words");
        assert_eq!(back.to_text(), text);
        let (long, bad) = Grants::from_text(&format!("pin {} {}\n", T + MAX_S + 1, T));
        assert_eq!((long.all().len(), bad), (0, 1), "a file cannot hold a grant longer than a day");
        assert_eq!(g.revoke(Some(Kind::Reload)).len(), 1);
        assert_eq!(g.revoke(None).len(), 1);
        assert!(g.all().is_empty());
    }
}
