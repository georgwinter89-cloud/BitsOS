//! The plain-text form for people and the desk: one line per window, then one per element, indented by depth.
//! `  toggle "Bewegung" = an [checked] @10,20,40,20 #w3/1x9k (press toggle)`.

use crate::{Element, Window};

fn line(e: &Element, depth: usize, out: &mut String) {
    let role = format!("{:?}", e.role).to_lowercase();
    out.push_str(&"  ".repeat(depth + 1));
    out.push_str(&format!("{role} {:?}", e.name));
    if let Some(v) = &e.value {
        out.push_str(&format!(" = {v:?}"));
    }
    if !e.state.is_empty() {
        let s: Vec<String> = e.state.iter().map(|s| format!("{s:?}").to_lowercase()).collect();
        out.push_str(&format!(" [{}]", s.join(" ")));
    }
    let [x, y, w, h] = e.rect;
    out.push_str(&format!(" @{x},{y},{w},{h}"));
    if !e.visible {
        out.push_str(" (verdeckt)");
    }
    if e.answers {
        out.push_str(" (bestätigt)");
    } else if e.confirm {
        out.push_str(" (fragt nach)");
    }
    out.push_str(&format!(" #{}", e.id));
    if !e.actions.is_empty() {
        let a: Vec<String> = e.actions.iter().map(|a| format!("{a:?}").to_lowercase()).collect();
        out.push_str(&format!(" ({})", a.join(" ")));
    }
    out.push('\n');
    for c in &e.children {
        line(c, depth + 1, out);
    }
}

pub fn render(windows: &[Window]) -> String {
    let mut out = String::new();
    for w in windows {
        let [x, y, ww, h] = w.rect;
        out.push_str(&format!(
            "window {} app={:?} title={:?} pid={} source={} @{x},{y},{ww},{h}{}\n",
            w.window,
            w.app,
            w.title,
            w.pid,
            format!("{:?}", w.source).to_lowercase(),
            if w.focused { " focused" } else { "" }
        ));
        for e in &w.elements {
            line(e, 0, &mut out);
        }
    }
    out
}
