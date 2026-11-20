//! VORRANG (K2): the habit "which priority Georg gives which program". The task manager tells ctxd when Georg
//! changed a program's Vorrang by his own input (`habit_event {kind:"vorrang", app, value}`); after the DB4
//! threshold (the last three the same, and most of the decayed ones) bitscomp starts that program so:
//! niedrig/normal imitated with the one-time hint and "Rückgängig", hoch only proposed (a card asks "Soll ...
//! immer mit hohem Vorrang starten?") and made only after Georg's own "Ja" (`vorrang_ja`). Never for BitsOS's
//! own services. Records: part `vorrang` (key app, val value), `ja` and `undo` (key the item id).

use crate::habits::Rec;
use crate::habits_rules::{Apply, FADED, FRESH_DAYS, IN_A_ROW, LAST, SHARE, weight};

pub const PART: &str = "vorrang";
pub const VALUES: [&str; 3] = ["niedrig", "normal", "hoch"];
/// BitsOS's own services (the programs init starts, and init): their Vorrang is the system's, never learned.
pub const OWN: [&str; 13] =
    ["init", "bitscomp", "ctxd", "settingsd", "bitsmcp", "bitsinstall", "bitsjournal", "bitspkgd", "bitspkg", "bitsaccess", "atspid", "bitssnap", "assist"];
/// The desktop's own windows (pages, the sofa mode) are bitscomp itself.
const OWN_PREFIX: &str = "bitscomp.";

pub fn own_service(app: &str) -> bool {
    app.starts_with(OWN_PREFIX) || OWN.contains(&app)
}

/// Who changed it: absent or "georg" is Georg's own input (the task manager sends no `by`); anything else (the
/// control socket's `ctl habits vorrang`, an event from a program that is not the desktop) is kept in the history
/// only and teaches nothing.
pub fn by_georg(by: Option<&str>) -> bool {
    matches!(by, None | Some("georg"))
}

/// What `by` becomes for a Vorrang event that did not come from the desktop: history only, like the control socket's.
pub const NOT_DESKTOP: &str = "fremd";

/// A habit event as ctxd takes it from a program that is not the desktop (`peer::desktop`): a `vorrang` event
/// teaches nothing whatever its `by` says (any program of the session could pose as the task manager), and Georg's
/// "Ja" to a high priority is refused (only bitscomp's card sees his real hand). Everything else is unchanged.
pub fn screened(a: &serde_json::Value, desktop: bool) -> Result<std::borrow::Cow<'_, serde_json::Value>, String> {
    if desktop {
        return Ok(std::borrow::Cow::Borrowed(a));
    }
    match a.get("kind").and_then(serde_json::Value::as_str).map(str::trim) {
        Some("vorrang_ja") => Err("only the desktop gives Georg's yes to a high Vorrang".into()),
        Some("vorrang") => {
            let mut b = a.clone();
            b["by"] = serde_json::Value::from(NOT_DESKTOP);
            Ok(std::borrow::Cow::Owned(b))
        }
        _ => Ok(std::borrow::Cow::Borrowed(a)),
    }
}

/// The record a `vorrang` event teaches: Err for a bad value or an own service, Ok(None) when it teaches nothing.
pub fn teaches(app: &str, value: Option<&str>, by: Option<&str>) -> Result<Option<(String, String)>, String> {
    if app.is_empty() {
        return Err("app is required".into());
    }
    let v = value.ok_or("value is required: niedrig, normal or hoch")?;
    if !VALUES.contains(&v) {
        return Err(format!("value {:?}: niedrig, normal or hoch", crate::util::cut_bytes(v, 40)));
    }
    if own_service(app) {
        return Err(format!("{:?} is a service of BitsOS: its Vorrang is not learned", crate::util::cut_bytes(app, 60)));
    }
    Ok(by_georg(by).then(|| (app.to_string(), v.to_string())))
}

/// What the page and bitscomp get for one program's records (`own` its `vorrang` records oldest first, `marks`
/// every record of the item: hints, undos, yeses).
pub struct Learned {
    pub title: String,
    pub reason: String,
    pub state: &'static str,
    pub count: u32,
    pub apply: Option<Apply>,
    pub hinted: bool,
}

fn how(v: &str) -> &'static str {
    match v {
        "niedrig" => "niedrigem",
        "hoch" => "hohem",
        _ => "normalem",
    }
}

pub fn learned(own: &[&Rec], marks: &[&Rec], now: i64, name: &str) -> Option<Learned> {
    let undo = marks.iter().filter(|r| r.part == "undo").max_by_key(|r| r.ts);
    let hinted = marks.iter().any(|r| r.part == "hint" || r.part == "ja");
    if let Some(u) = undo {
        let reason = if u.val.as_deref() == Some("nein") { "weil du „Nein“ gesagt hast" } else { "weil du beim Nachmachen „Rückgängig“ gewählt hast" };
        return Some(Learned { title: format!("{name}: Vorrang wird nicht mehr nachgemacht"), reason: reason.into(), state: "abgelehnt", count: own.len() as u32, apply: None, hinted });
    }
    let last: Vec<&Rec> = own.iter().rev().take(LAST).copied().collect();
    let s = last.first()?.val.clone()?;
    let row = last.len() >= IN_A_ROW && last[..IN_A_ROW].iter().all(|r| r.val.as_deref() == Some(s.as_str()));
    let held = row && last[..IN_A_ROW].iter().map(|r| weight(r.ts, now)).sum::<f64>() >= IN_A_ROW as f64 * FADED;
    let total: f64 = own.iter().map(|r| weight(r.ts, now)).sum();
    let same: f64 = own.iter().filter(|r| r.val.as_deref() == Some(s.as_str())).map(|r| weight(r.ts, now)).sum();
    if !held || total <= 0.0 || same / total < SHARE {
        return None;
    }
    let recent: Vec<&Rec> = last.iter().copied().filter(|r| now - r.ts <= FRESH_DAYS * 86_400).collect();
    let k = recent.iter().filter(|r| r.val.as_deref() == Some(s.as_str())).count() as u32;
    let counted = format!("{k} von {} Mal so eingestellt", recent.len());
    let apply = |ask| Some(Apply::Vorrang { value: s.clone(), ask });
    if s != "hoch" {
        return Some(Learned { title: format!("{name} startet mit {} Vorrang", how(&s)), reason: format!("weil du das {counted} hast"), state: "nachgemacht", count: k, apply: apply(false), hinted });
    }
    // A yes counts for this run of "hoch" only, given once it was proposed (at the run's third): one from before
    // Georg chose something else, or from before the proposal, asks again.
    let start = own.iter().rposition(|r| r.val.as_deref() != Some("hoch")).map_or(0, |i| i + 1);
    let proposed = own.get(start + IN_A_ROW - 1).map_or(i64::MAX, |r| r.ts);
    if marks.iter().any(|r| r.part == "ja" && r.ts >= proposed) {
        return Some(Learned { title: format!("{name} startet mit hohem Vorrang"), reason: format!("weil du das {counted} und „Ja“ gesagt hast"), state: "nachgemacht", count: k, apply: apply(false), hinted });
    }
    Some(Learned { title: format!("{name} immer mit hohem Vorrang starten"), reason: format!("weil du das {counted} hast"), state: "vorgeschlagen", count: k, apply: apply(true), hinted })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_three_values_and_never_an_own_service() {
        assert_eq!(teaches("brave", Some("niedrig"), None), Ok(Some(("brave".into(), "niedrig".into()))));
        assert_eq!(teaches("brave", Some("hoch"), Some("georg")), Ok(Some(("brave".into(), "hoch".into()))));
        assert_eq!(teaches("brave", Some("normal"), Some("ctl")), Ok(None), "the control socket teaches nothing");
        for bad in ["Hoch", "high", "", "niedrig ", "-5"] {
            assert!(teaches("brave", Some(bad), None).is_err(), "{bad:?}");
        }
        assert!(teaches("brave", None, None).is_err());
        assert!(teaches("", Some("hoch"), None).is_err());
        for own in ["ctxd", "settingsd", "bitscomp.einstellungen", "init", "assist"] {
            assert!(teaches(own, Some("niedrig"), None).is_err(), "{own}");
        }
    }
}
