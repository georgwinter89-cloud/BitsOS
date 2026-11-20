//! Asked steps (BITS.md "Fragen"). bitscomp answers a step it must ask Georg about with `frage <n> <s>`: question
//! number n, open for at most s seconds. `bits` then asks `bits-antwort <n>` until Georg answered on the desktop
//! (only his real mouse or keyboard says yes), and after a yes sends `bits-mit <n> <the same line>`: bitscomp runs
//! exactly that line once. A question `bits` stops waiting for is withdrawn (`bits-antwort <n> nein`: anyone may
//! say no).

use std::time::{Duration, Instant};

use crate::out::{Fail, R};
use crate::sock;

/// How often the answer is asked.
const POLL: Duration = Duration::from_millis(250);

pub const NO_ANSWER: &str = "Georg hat nicht geantwortet. Es wurde nichts getan.";
pub const SAID_NO: &str = "Georg hat Nein gesagt. Es wurde nichts getan.";

/// How long `bits` waits for an answer: `BITS_FRAGE_S` (seconds, at most the question's own time), else the
/// question's time.
fn own_wait() -> Option<Duration> {
    std::env::var("BITS_FRAGE_S").ok()?.parse::<f64>().ok().filter(|s| *s >= 0.0).map(Duration::from_secs_f64)
}

/// A question bitscomp opened: `frage <n> <seconds>`.
pub fn parse(answer: &str) -> Option<(u64, u64)> {
    let mut w = answer.strip_prefix("frage ")?.split_whitespace();
    Some((w.next()?.parse().ok()?, w.next()?.parse().ok()?))
}

/// Sends a line that may be asked; Ok is the answer of the line itself.
pub fn changing(line: &str) -> R<String> {
    let first = sock::ctl(line)?;
    let Some((n, secs)) = parse(&first) else { return Ok(first) };
    wait_yes(n, secs)?;
    sock::ctl(&format!("bits-mit {n} {line}"))
}

/// A question that runs nothing in bitscomp (a setting, the trash): Ok after Georg's yes.
pub fn ask(line: &str) -> R<()> {
    let first = sock::ctl(line)?;
    let Some((n, secs)) = parse(&first) else {
        return Err(Fail::failed(format!("Die Oberfläche hat nicht gefragt: {first}")));
    };
    wait_yes(n, secs)
}

fn wait_yes(n: u64, secs: u64) -> R<()> {
    let theirs = Duration::from_secs(secs);
    let limit = own_wait().map_or(theirs, |w| w.min(theirs));
    let start = Instant::now();
    loop {
        match sock::ctl(&format!("bits-antwort {n}"))?.as_str() {
            "ja" => return Ok(()),
            "nein" => return Err(Fail::refused(SAID_NO)),
            "wartet" => {}
            _ => return Err(Fail::refused(NO_ANSWER)),
        }
        if start.elapsed() >= limit {
            let _ = sock::ctl(&format!("bits-antwort {n} nein"));
            return Err(Fail::refused(NO_ANSWER));
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_question_is_number_and_seconds() {
        assert_eq!(super::parse("frage 7 120"), Some((7, 120)));
        assert_eq!(super::parse("ok"), None);
        assert_eq!(super::parse("frage x 1"), None);
    }
}
