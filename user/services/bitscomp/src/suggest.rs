//! The address suggestions under the address bar while it is edited (Details.dc.html, WY-5): grouped as
//! favourites (first, desktop.md §1a), programs, settings keys (from settingsd: key and its German text) and
//! recently used. Files and contexts come with the address layer later. Pure: rows from the inputs, geometry in
//! logical pixels.

use crate::layout::Rect;
use crate::state::App;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub group: &'static str,
    pub name: String,
    pub addr: String,
}

/// The programs BitsOS can start from an address today.
const PROGRAMS: [(&str, &str); 1] = [("Terminal", "programm/terminal")];

pub const ROW_H: i32 = 36;
pub const HEAD_H: i32 = 26;

/// The name a favourite or recent address shows: its last part, capitalised ("programm/terminal" -> "Terminal").
pub fn short_name(addr: &str) -> String {
    let last = addr.trim_end_matches('/').rsplit('/').next().unwrap_or(addr);
    let mut c = last.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
}

/// Every row that matches `query` (case-insensitive, in name or address); an empty query lists favourites,
/// programs and recent addresses but not the many settings keys.
pub fn build(query: &str, favourites: &[String], keys: &[(String, String)], recent: &[String]) -> Vec<Row> {
    let q = query.trim().to_lowercase();
    let hit = |name: &str, addr: &str| q.is_empty() || name.to_lowercase().contains(&q) || addr.to_lowercase().contains(&q);
    let mut out: Vec<Row> = Vec::new();
    let push = |out: &mut Vec<Row>, group, name: String, addr: String| {
        if hit(&name, &addr) && !out.iter().any(|r| r.addr == addr) {
            out.push(Row { group, name, addr });
        }
    };
    for f in favourites {
        push(&mut out, "FAVORITEN", short_name(f), f.clone());
    }
    for (n, a) in PROGRAMS {
        push(&mut out, "PROGRAMME", n.into(), a.into());
    }
    if !q.is_empty() {
        for (k, text) in keys {
            let name = if text.is_empty() { k.clone() } else { text.clone() };
            push(&mut out, "EINSTELLUNGEN", name, format!("einstellungen/{k}"));
        }
    }
    for r in recent {
        push(&mut out, "ZULETZT BENUTZT", short_name(r), r.clone());
    }
    out
}

/// Where the rows go below `field`, down to `bottom`: (group heading, row) per row; rows that do not fit are left out.
pub fn layout(rows: &[Row], field: Rect, bottom: i32) -> (Rect, Vec<(Option<Rect>, Rect)>) {
    let x = field.x;
    let w = field.w.min(720);
    let top = field.y + field.h + 6;
    let mut y = top;
    let mut out = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let head = i == 0 || rows[i - 1].group != r.group;
        let need = ROW_H + if head { HEAD_H } else { 0 };
        if y + need > bottom {
            break;
        }
        let h = head.then(|| Rect { x, y, w, h: HEAD_H });
        y += if head { HEAD_H } else { 0 };
        out.push((h, Rect { x, y, w, h: ROW_H }));
        y += ROW_H;
    }
    (Rect { x, y: top, w, h: y - top }, out)
}

impl App {
    /// The suggestions shown for the address bar being edited (only as many as fit on the output).
    pub fn suggestions(&self) -> Vec<Row> {
        let Some(e) = &self.desk.edit else { return Vec::new() };
        let rows = build(&e.text, &self.desk.favourites, &self.setting_keys, &self.session.recent_addresses());
        let (_, placed) = layout(&rows, self.edit_field(), self.area_full().h - 8);
        rows.into_iter().take(placed.len()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn favourites_come_first_and_duplicates_once() {
        let keys = vec![("theme.accent".to_string(), "Akzentfarbe".to_string())];
        let rows = build("", &s(&["programm/terminal", "einstellungen/aussehen"]), &keys, &s(&["programm/terminal"]));
        let groups: Vec<_> = rows.iter().map(|r| r.group).collect();
        assert_eq!(groups, ["FAVORITEN", "FAVORITEN"], "programm/terminal is a favourite already; no keys without a query");
        let rows = build("akzent", &[], &keys, &[]);
        assert_eq!(rows, [Row { group: "EINSTELLUNGEN", name: "Akzentfarbe".into(), addr: "einstellungen/theme.accent".into() }]);
        assert_eq!(build("TERM", &[], &[], &[])[0].addr, "programm/terminal");
        assert_eq!(short_name("einstellungen/aussehen"), "Aussehen");
    }

    #[test]
    fn rows_stop_at_the_bottom() {
        let rows = build("", &s(&["a/b", "c/d", "e/f"]), &[], &s(&["g/h"]));
        let field = Rect { x: 0, y: 0, w: 400, h: 40 };
        let (_, all) = layout(&rows, field, 10_000);
        assert_eq!(all.len(), rows.len());
        assert!(all[0].0.is_some() && all[1].0.is_none(), "a heading per group");
        let (_, few) = layout(&rows, field, 46 + HEAD_H + 2 * ROW_H);
        assert_eq!(few.len(), 2);
    }
}
