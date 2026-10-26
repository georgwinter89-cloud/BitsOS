//! Control socket commands of WY-5, so tests drive the desktop without a hand:
//!
//! ws | ws new | ws N (show workspace N, 1-based) | ws remove
//! address [TEXT] (edit the top bar, with TEXT typed) | address open TEXT [tab|tile]
//! sidebar (fold Claude's sidebar in or out) | ring idle|listening|thinking|working|waiting
//! workspaces (fold the workspace list in or out) | reveal left|right|none (show a folded bar over the tiles, as the
//! pointer at its edge does) | bars (the two side bars: docked, folded or revealed)
//! theme (the applied theme in one line) | set KEY VALUE | unset KEY (a local settings value, as if settingsd sent it)
//! start (show the start page) | session (what would be saved) | restore

use crate::address::Where;
use crate::desk::Ring;
use crate::state::App;
use crate::wm::Action;

pub fn execute(app: &mut App, words: &[&str], line: &str) -> Option<Result<String, String>> {
    let rest = |n: usize| line.split_whitespace().skip(n).collect::<Vec<_>>().join(" ");
    let r = match words.first().copied()? {
        "ws" => match words.get(1).copied() {
            None => Ok(ws_list(app)),
            Some("new") => Ok(format!("ws {}", app.ws_new() + 1)),
            Some("remove") => {
                if app.ws_remove(app.desk.active) { Ok("ok".into()) } else { Err("only an empty workspace that is not the only one".into()) }
            }
            Some(n) => match n.parse::<usize>() {
                Ok(i) if i >= 1 && i <= app.ws_count() => {
                    app.act(Action::Workspace(i - 1));
                    Ok(format!("ws {i}"))
                }
                _ => Err(format!("no workspace {n}")),
            },
        },
        "address" => match words.get(1).copied() {
            Some("open") => {
                let (text, w) = match words.last().copied() {
                    Some("tile") if words.len() > 3 => (words[2..words.len() - 1].join(" "), Where::Tile),
                    Some("tab") if words.len() > 3 => (words[2..words.len() - 1].join(" "), Where::Tab),
                    _ => (rest(2), Where::Tab),
                };
                app.open_address(&text, w, None);
                Ok(app.desk.notice.as_ref().map_or("ok".into(), |(n, _)| format!("notice {n}")))
            }
            _ => {
                app.address_begin(None);
                let typed = rest(1);
                if let Some(e) = app.desk.edit.as_mut()
                    && !typed.is_empty()
                {
                    e.text = typed.clone();
                    e.cursor = typed.chars().count();
                }
                app.chrome_dirty();
                Ok("editing".into())
            }
        },
        // The notice line, and `notice clear` (tests: a notice runs out after 5 s, which a screenshot must not race).
        "notice" => {
            if words.get(1) == Some(&"clear") && app.desk.notice.take().is_some() {
                app.chrome_dirty();
            }
            Ok(app.desk.notice.as_ref().map_or("notice none".into(), |(n, _)| format!("notice {n}")))
        }
        // The address bar's text while it is edited (tests of the key repeat).
        "edit" => Ok(match &app.desk.edit {
            Some(e) => format!("edit text={:?} cursor={} held={}", e.text, e.cursor, app.desk.held.is_some()),
            None => "edit none".into(),
        }),
        "sidebar" => {
            app.act(Action::Sidebar);
            Ok(format!("sidebar {}", if app.theme.layout.sidebar { "open" } else { "folded" }))
        }
        "workspaces" => {
            app.act(Action::Workspaces);
            Ok(format!("workspaces {}", if app.theme.layout.workspaces { "open" } else { "folded" }))
        }
        "reveal" => {
            let ch = app.chrome();
            match words.get(1).copied() {
                Some("left") if !ch.list_docked => app.desk.reveal.show(crate::reveal::Edge::Left),
                Some("right") if !ch.side_docked => app.desk.reveal.show(crate::reveal::Edge::Right),
                Some("none") => {
                    app.desk.reveal.clear();
                }
                Some("left" | "right") => return Some(Err("that bar is docked, nothing to reveal".into())),
                _ => return Some(Err("reveal left|right|none".into())),
            }
            app.chrome_dirty();
            app.layout_dirty = true;
            Ok(bars(app))
        }
        "bars" => Ok(bars(app)),
        "ring" => {
            let r = match words.get(1).copied() {
                Some("idle") => Ring::Idle,
                Some("listening") => Ring::Listening,
                Some("thinking") => Ring::Thinking,
                Some("working") => Ring::Working,
                Some("waiting") => Ring::Waiting,
                _ => return Some(Err("ring idle|listening|thinking|working|waiting".into())),
            };
            app.set_ring(r);
            Ok(format!("ring {} ({})", r.word(), crate::ring::hint(r)))
        }
        "theme" => Ok(theme_line(app)),
        // WY-5b: the settings pages and where foot's colours are.
        "page" => {
            // `page clear`: the answer lines go (so a picture after undo can equal the first one).
            if words.get(1) == Some(&"clear") {
                app.page_clear_msgs();
            }
            Ok(app.page_report())
        }
        "foot" => Ok(format!(
            "foot config={} ready={} dark={}",
            app.foot.path.as_ref().map_or("-".to_string(), |p| p.display().to_string()),
            app.foot.ready,
            app.theme.dark
        )),
        "set" if words.len() >= 3 => {
            app.settings.set(words[1], &rest(2));
            app.apply_settings(words[1]);
            Ok(theme_line(app))
        }
        "unset" if words.len() == 2 => {
            app.settings.remove(words[1]);
            app.apply_settings(words[1]);
            Ok(theme_line(app))
        }
        "start" => {
            app.desk.start_forced = true;
            app.chrome_dirty();
            Ok("ok".into())
        }
        "session" => Ok(crate::session::encode(&app.session_now(), &app.session.recent)),
        "restore" => {
            app.session_restore();
            Ok("ok".into())
        }
        _ => return None,
    };
    Some(r)
}

/// "bars workspaces=docked|folded|revealed sidebar=…".
fn bars(app: &App) -> String {
    let ch = app.chrome();
    let state = |docked: bool, shown: bool| if docked { "docked" } else if shown { "revealed" } else { "folded" };
    format!(
        "bars workspaces={} sidebar={}",
        state(ch.list_docked, ch.list_shown),
        state(ch.side_docked, ch.side.is_some())
    )
}

fn ws_list(app: &App) -> String {
    let mut out = String::new();
    for i in 0..app.ws_count() {
        let (tiles, tabs) = app.ws_counts(i);
        out.push_str(&format!(
            "ws {} active={} tiles={tiles} tabs={tabs} name={:?}\n",
            i + 1,
            i == app.desk.active,
            app.desk.names[i]
        ));
    }
    out.push_str("end");
    out
}

fn theme_line(app: &App) -> String {
    let t = &app.theme;
    let hex = |c: [f32; 4]| format!("#{:02x}{:02x}{:02x}", (c[0] * 255.0).round() as u8, (c[1] * 255.0).round() as u8, (c[2] * 255.0).round() as u8);
    format!(
        "mode={} accent={} bg={} scale={} motion={:?} effects={} address={} favourites={} sidebar={} statusbar={} workspaces={} name={:?} settingsd={} gen={}",
        if t.dark { "dark" } else { "light" },
        hex(t.accent),
        hex(t.bg),
        t.scale,
        t.motion,
        if t.effects_gpu { "gpu" } else { "cheap" },
        if t.layout.address_top { "top" } else { "tile" },
        t.layout.favourites,
        t.layout.sidebar,
        t.layout.statusbar,
        t.layout.workspaces,
        app.desk.claude_name,
        app.settings_live,
        app.theme_gen
    )
}
