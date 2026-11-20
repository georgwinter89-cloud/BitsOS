//! EINGABE2: which editable element of a window has the keyboard. Chromium says FOCUSED for the element focused
//! inside the web page and also for the browser's own field that really has the keyboard (its address bar); the
//! page's word stays while the bar has the keyboard, the bar's goes when the keyboard leaves it (Brave on the
//! Mini-PC, 14.11.). So the target saying FOCUSED proves nothing alone: the whole window is walked. A field outside
//! any web page has the keyboard when no other such field says FOCUSED (a page's focused element is only the page's
//! focus then); a field inside a page has it only when no other editable element of the window says FOCUSED. An
//! element around the target or inside it (an editable combobox around its entry) is the same field.

use crate::atspi::{self, state};
use crate::read::Reader;
use std::collections::HashMap;

/// The most objects one walk looks at (as a read of the tree).
const MAX_WALK: usize = crate::read::MAX_NODES;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Proof {
    /// The target is the one editable element with the focus.
    Only,
    /// The target does not say FOCUSED.
    NotFocused,
    /// The target says it, and so does another editable element (its name).
    Other(String),
    /// The window could not be seen whole (a call failed, the walk was cut, the target was not met): no proof.
    Unknown,
}

/// An editable element that says FOCUSED: its path, its name, and whether it lies inside a web page.
pub type Focused = (String, String, bool);

/// Whether `a` lies on the way from `b` up to the window, by the walk's parents.
fn above(parents: &HashMap<String, String>, a: &str, b: &str) -> bool {
    let mut p = parents.get(b);
    for _ in 0..crate::read::MAX_DEPTH {
        match p {
            Some(x) if x == a => return true,
            Some(x) => p = parents.get(x),
            None => return false,
        }
    }
    false
}

/// The rule, without the bus: `focused` are the editable elements that say FOCUSED (the target among them or not);
/// `in_page`: the target lies inside a web page.
pub fn judge(target: &str, target_focused: bool, in_page: bool, focused: &[Focused], parents: &HashMap<String, String>) -> Proof {
    if !target_focused {
        return Proof::NotFocused;
    }
    let same = |p: &str| p == target || above(parents, p, target) || above(parents, target, p);
    match focused.iter().find(|(p, _, page)| !same(p) && (in_page || !page)) {
        Some((_, name, _)) => Proof::Other(name.clone()),
        None => Proof::Only,
    }
}

fn page_role(r: u32) -> bool {
    matches!(r, crate::atspi::role::DOCUMENT_WEB | crate::atspi::role::DOCUMENT_FRAME)
}

/// Why a walk gives no proof.
#[derive(Debug, PartialEq, Eq)]
pub enum Cut {
    Call,
    Size,
    NoTarget,
}

/// Outcome of a walk: `Err` when the window was not seen whole (fail closed, checkers EINGABE2).
pub fn conclude(cut: Option<Cut>, met_target: bool) -> Result<(), Cut> {
    match cut {
        Some(c) => Err(c),
        None if !met_target => Err(Cut::NoTarget),
        None => Ok(()),
    }
}

/// Walks the window `frame` (what is on screen) and judges `target`. The parts outside any web page (the browser's
/// own bar) are walked first and whole, so a big page cannot use up the budget before the bar is seen; anything not
/// seen whole gives `Unknown`.
pub async fn proof(r: &Reader<'_>, frame: &str, target: &str) -> Proof {
    match walk(r, frame, target).await {
        Ok(p) => p,
        Err(c) => {
            eprintln!("atspid: focus proof: no proof ({c:?})");
            Proof::Unknown
        }
    }
}

async fn walk(r: &Reader<'_>, frame: &str, target: &str) -> Result<Proof, Cut> {
    let target_states = r.try_states(target).await.map_err(|_| Cut::Call)?;
    if !atspi::has(target_states, state::FOCUSED) {
        return Ok(Proof::NotFocused);
    }
    let mut parents = HashMap::new();
    let mut focused = Vec::new();
    let mut met = frame == target;
    let mut target_in_page = false;
    // ui: outside any page; pages: inside one. The ui stack is emptied first.
    let mut ui = vec![(frame.to_string(), 0u32)];
    let mut pages: Vec<(String, u32)> = Vec::new();
    let mut seen = 0;
    let mut cut = None;
    while let Some((p, depth, in_page)) = ui.pop().map(|(p, d)| (p, d, false)).or_else(|| pages.pop().map(|(p, d)| (p, d, true))) {
        seen += 1;
        if seen > MAX_WALK || depth > crate::read::MAX_DEPTH {
            cut = Some(Cut::Size);
            break;
        }
        let Ok(all) = r.look(&p).await else {
            cut = Some(Cut::Call);
            break;
        };
        let kids: Vec<String> = all.into_iter().filter(|(b, _)| b.is_empty() || b == r.app).map(|(_, c)| c).collect();
        let looks = futures_util::future::join_all(kids.iter().map(|c| async { (r.try_states(c).await, r.role(c).await) })).await;
        for (c, (st, role)) in kids.into_iter().zip(looks) {
            let (Ok(st), Some(role)) = (st, role) else {
                cut = Some(Cut::Call);
                break;
            };
            if !atspi::has(st, state::SHOWING) && !atspi::has(st, state::FOCUSED) {
                continue;
            }
            parents.insert(c.clone(), p.clone());
            if c == target {
                met = true;
                target_in_page = in_page;
            }
            if atspi::has(st, state::FOCUSED) && atspi::has(st, state::EDITABLE) {
                focused.push((c.clone(), r.name(&c).await, in_page));
            }
            if in_page || page_role(role) {
                pages.push((c, depth + 1));
            } else {
                ui.push((c, depth + 1));
            }
        }
        if cut.is_some() {
            break;
        }
    }
    conclude(cut, met)?;
    Ok(judge(target, atspi::has(target_states, state::FOCUSED), target_in_page, &focused, &parents))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_must_be_the_only_editable_with_the_focus() {
        let parents: HashMap<String, String> = [("/url", "/ui"), ("/ui", "/f"), ("/doc", "/f"), ("/box", "/doc"), ("/entry", "/box")]
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        let f = |v: &[&str]| v.iter().map(|p| (p.to_string(), format!("name{p}"), *p != "/url")).collect::<Vec<_>>();
        assert_eq!(judge("/box", true, true, &f(&["/url", "/box"]), &parents), Proof::Other("name/url".into()), "Chromium: the address bar still has the keyboard");
        assert_eq!(judge("/box", true, true, &f(&["/box"]), &parents), Proof::Only);
        assert_eq!(judge("/box", true, true, &f(&["/box", "/entry"]), &parents), Proof::Only, "its own entry is the same field");
        assert_eq!(judge("/entry", true, true, &f(&["/box", "/entry"]), &parents), Proof::Only, "the combobox around it too");
        assert_eq!(judge("/box", false, true, &f(&["/url"]), &parents), Proof::NotFocused);
        assert_eq!(judge("/url", true, false, &f(&["/url", "/box"]), &parents), Proof::Only, "the bar has it; the page's word is only the page's focus");
        let two_bars = vec![("/url".to_string(), "a".to_string(), false), ("/ui".to_string(), "b".to_string(), false)];
        let p2: HashMap<String, String> = [("/url", "/f"), ("/ui", "/f")].iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        assert_eq!(judge("/url", true, false, &two_bars, &p2), Proof::Other("b".into()), "outside a page: one only");
    }

    #[test]
    fn a_window_not_seen_whole_gives_no_proof() {
        assert_eq!(conclude(Some(Cut::Size), true), Err(Cut::Size), "the walk was cut: the bar may be unseen");
        assert_eq!(conclude(Some(Cut::Call), true), Err(Cut::Call), "a failed call is not 'not showing'");
        assert_eq!(conclude(None, false), Err(Cut::NoTarget), "the target was never met");
        assert_eq!(conclude(None, true), Ok(()));
    }
}
