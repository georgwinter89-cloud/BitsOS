//! SYS-A: `ctl system ...` for scenarios and the Mini-PC checks. Without words: the state and the shown tab as text.
//! `tab WORD`, `range 1min|1h|24h`, `filter alle|fenster|hintergrund|bitsos`, `sort COLUMN`, `search TEXT`,
//! `select PID`, `device N` change the newest (or focused) System window; `press PART` clicks a control as
//! injected input (what is asked stays asked: `press` cannot answer a question, `confirm` neither, as MCP);
//! `rows` the task manager's table; `report` the clipboard report; `copy` puts it on the clipboard; `mask on|off`
//! draws values as blocks (reference pictures); `shot FILE` writes the window's own picture as PPM.

use crate::state::App;
use crate::sys_group::{Column, Filter};
use crate::sys_ring::Range;
use crate::sys_rules::Vorrang;
use crate::sys_view::{SysPart, Tab};

fn part_of(w: &str) -> Option<SysPart> {
    let (k, v) = w.split_once('=').unwrap_or((w, ""));
    Some(match k {
        "tab" => SysPart::Tab(Tab::of_word(v)?),
        "range" => SysPart::Range(Range::of_word(v)?),
        "filter" => SysPart::Filter(Filter::ALL.into_iter().find(|f| f.word() == v)?),
        "sort" => SysPart::Sort(Column::ALL.into_iter().find(|c| c.word() == v)?),
        "row" => SysPart::Row(v.parse().ok()?),
        "more" => SysPart::More,
        "show" => SysPart::Show,
        "end" => SysPart::End,
        "vorrang" => SysPart::Prio(Vorrang::of_word(v)?),
        "werhatwas" => SysPart::WhoDid,
        "device" => SysPart::Device(v.parse().ok()?),
        "copy" => SysPart::Copy,
        "link" => SysPart::Link(v.parse().ok()?),
        "search" => SysPart::Search,
        _ => return None,
    })
}

fn ppm(c: &crate::canvas::Canvas, path: &str) -> std::io::Result<()> {
    let mut buf = format!("P6\n{} {}\n255\n", c.w, c.h).into_bytes();
    for px in c.bytes().chunks_exact(4) {
        // Argb8888 little endian: b, g, r, a.
        buf.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    std::fs::write(path, buf)
}

pub fn ctl(app: &mut App, words: &[&str]) -> Option<Result<String, String>> {
    if words.first().copied()? != "system" {
        return None;
    }
    let page = app.sys_page_for_ctl();
    let need = |p: Option<u32>| p.ok_or_else(|| "system: no System window is open (address `system`)".to_string());
    let r = match words.get(1).copied() {
        None | Some("text") => need(page).map(|p| app.sys_text(p)),
        Some("tab") => need(page).and_then(|p| {
            let t = words.get(2).and_then(|w| Tab::of_word(w)).ok_or("system tab: want a tab word (überblick, prozessor, ..., verlauf)")?;
            app.sys_set_tab(p, t);
            Ok(format!("tab {}", t.word()))
        }),
        Some("press") => need(page).and_then(|p| {
            let part = words.get(2).and_then(|w| part_of(w)).ok_or("system press: want a part as `ctl hot` names it (end, vorrang=niedrig, row=PID, ...)")?;
            app.pages.real = false;
            app.sys_act(p, part);
            Ok(format!("pressed {}", crate::sys_view::part_name(part)))
        }),
        Some("confirm") => need(page).and_then(|p| {
            if app.sys_yes_early(p) {
                return Err("system confirm: too early, the question takes a yes only after 1 s".to_string());
            }
            app.pages.real = false;
            app.page_dialog_close(p, true);
            Ok(app.pages.by_pid.get(&p).and_then(|s| s.msg.clone()).map_or_else(|| "confirmed".into(), |(m, e)| format!("{}{m}", if e { "error: " } else { "" })))
        }),
        Some(w @ ("range" | "filter" | "sort" | "select" | "device")) => need(page).and_then(|p| {
            let mut arg = words.get(2).copied().unwrap_or("").to_string();
            // `select PID`: the row that holds the process (a helper chooses its program's row).
            if w == "select"
                && let Some(r) = arg.parse::<u32>().ok().and_then(|pid| app.sys.rows.iter().find(|r| r.pids.contains(&pid)))
            {
                arg = r.main().to_string();
            }
            let key = match w {
                "select" => "row",
                x => x,
            };
            let part = part_of(&format!("{key}={arg}")).ok_or(format!("system {w}: unknown {arg:?}"))?;
            app.pages.real = false;
            app.sys_act(p, part);
            Ok(format!("{w} {arg}"))
        }),
        Some("search") => need(page).map(|p| {
            let q = words[2..].join(" ");
            if let Some(s) = app.pages.by_pid.get_mut(&p) {
                s.sys.query = q.clone();
                s.scroll = 0;
            }
            app.needs_frame = true;
            format!("search {q:?}")
        }),
        Some("rows") => need(page).map(|p| {
            let v = app.pages.by_pid.get(&p).map(|s| s.sys.clone()).unwrap_or_default();
            let mut t = String::new();
            for r in &app.sys.rows {
                t.push_str(&format!(
                    "{:?} | {} | {} | pids={} | uid={} | nice={} | state={} | cpu={} | app={} | wins={}\n",
                    r.part.map_or(crate::sys_group::Part::Background, |x| x),
                    r.name,
                    r.main(),
                    r.pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(","),
                    r.uid.map_or_else(|| "-".into(), |u| u.to_string()),
                    r.nice,
                    r.state.map_or("", |s| s.word()),
                    crate::sys_text::pct(r.cpu),
                    r.app,
                    r.wins.len()
                ));
            }
            format!("rows={} filter={} sort={}\n{t}end", app.sys.rows.len(), v.filter.word(), v.sort.word())
        }),
        Some("report") => Ok(crate::sys_report::text(&app.sys.snap, &app.sys.desk, crate::werhatwas_app::clock(app.status.fixed).1)),
        Some("copy") => {
            let t = crate::sys_report::text(&app.sys.snap, &app.sys.desk, crate::werhatwas_app::clock(app.status.fixed).1);
            app.sys_clip_set(t.clone());
            Ok(t)
        }
        Some("mask") => need(page).map(|_| {
            let on = words.get(2) != Some(&"off");
            for s in app.pages.by_pid.values_mut() {
                s.sys.mask = on;
            }
            app.needs_frame = true;
            format!("mask {on}")
        }),
        Some("shot") => need(page).and_then(|p| {
            let path = words.get(2).ok_or("system shot FILE")?;
            let w = app.sys_page_window(p).ok_or("system shot: the window is not mapped")?;
            let frame = app.drawn_placements().into_iter().find(|x| x.win == w).map(|x| x.frame).ok_or("system shot: the window is not placed")?;
            let content = app.content_rect(frame);
            app.sys_prepare(p, content);
            let look = app.sys_look(p, content).ok_or("system shot: no look")?;
            let c = crate::sys_draw::draw(&look, &app.theme);
            ppm(&c, path).map_err(|e| e.to_string())?;
            Ok(format!("{}x{}", c.w, c.h))
        }),
        Some(w) => Err(format!("system: unknown word {w}")),
    };
    Some(r)
}

impl App {
    /// The state line and the shown tab as text.
    pub(crate) fn sys_text(&self, page: u32) -> String {
        let Some(s) = self.pages.by_pid.get(&page) else { return String::new() };
        let v = &s.sys;
        let m = self.sys_model(v);
        let msg = s.msg.as_ref().map_or("none".to_string(), |(t, e)| format!("{}{t:?}", if *e { "error:" } else { "" }));
        let dialog = s.confirm.as_ref().map_or("none".to_string(), |d| format!("{:?}:{:?}", d.title, d.ok));
        format!(
            "system page={page} address={} tab={} range={} samples={} procs={} rows={} selected={} filter={} sort={}{} query={:?} journal={} mask={} msg={msg} dialog={dialog} pending={} rlimit_nice={}\n{}end",
            s.address,
            v.tab.word(),
            v.range.word(),
            self.sys.samples,
            self.sys.snap.procs.len(),
            self.sys.rows.len(),
            v.selected.map_or_else(|| "-".into(), |(p, _)| p.to_string()),
            v.filter.word(),
            v.sort.word(),
            if v.up { "+up" } else { "" },
            v.query,
            self.sys.journal.map_or("-", |j| if j { "yes" } else { "no" }),
            v.mask,
            self.sys.pending.len(),
            crate::sys_limit::current().map_or_else(|| "-".into(), |n| n.to_string()),
            crate::sys_report::tab_text(&m)
        )
    }

    /// `ctl page`'s words for a System window.
    pub(crate) fn sys_report(&self, page: u32) -> String {
        self.pages.by_pid.get(&page).map_or_else(String::new, |s| format!(" system tab={} samples={} asking={}", s.sys.tab.word(), self.sys.samples, self.sys_asks(page)))
    }
}
