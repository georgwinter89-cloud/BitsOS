//! `bits meldung <text>`: one line on the desktop's notice bar (bitscomp's `bits-meldung`); from the assistant's
//! tree it is marked as the assistant's.

use crate::out::{Done, Fail, R};
use crate::sock;

pub fn meldung(args: &[String]) -> R {
    let text = args.join(" ");
    if text.trim().is_empty() {
        return Err(Fail::usage("meldung <text>"));
    }
    if text.contains('\n') {
        return Err(Fail::usage("Eine Meldung ist eine Zeile."));
    }
    let shown = sock::ctl(&format!("bits-meldung {text}"))?;
    Ok(Done::new(shown.clone()).with("shown", shown))
}
