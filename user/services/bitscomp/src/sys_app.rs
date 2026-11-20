//! SYS-A: the program "System" in bitscomp: page windows of kind `System` (page_kind) whose generic part (window,
//! scroll, the search field, the dialog, the image) is page_app's. A worker thread samples once a second while at
//! least one System window is open and sleeps otherwise; each snapshot feeds the history, the task manager's rows
//! (grouped with bitscomp's own window list) and the desktop's frame numbers, and redraws the windows.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, Event};

use crate::desk::Spot;
use crate::layout::Rect;
use crate::page::Part;
use crate::page_kind::PageKind;
use crate::state::App;
use crate::sys_draw::SysLook;
use crate::sys_group::{self as grp, Row, Win};
use crate::sys_model::{self as model, Desk, TabModel};
use crate::sys_ring::{History, Range};
use crate::sys_sample::{Sampler, Want};
use crate::sys_snap::Snapshot;
use crate::sys_sources::Root;
use crate::sys_view::{Tab, View};

pub enum Msg {
    Snap(Box<Snapshot>),
    /// The journal's 24 h points by the page's series names (None: not readable).
    Day(Option<Vec<(String, Vec<Option<f64>>)>>),
}

/// A polite end on its way: after `WAIT` the page offers "Sofort beenden".
#[derive(Clone, Debug, PartialEq)]
pub struct Pending {
    pub page: u32,
    pub pid: u32,
    pub start: u64,
    pub name: String,
    pub at: f64,
    pub offered: bool,
}

/// What an open question of a page will do on its yes.
#[derive(Clone, Debug, PartialEq)]
pub enum Asked {
    End { pid: u32, start: u64, kill: bool },
    Prio { pid: u32, start: u64, v: crate::sys_rules::Vorrang },
}

pub const WAIT: f64 = 3.0;

pub struct Sys {
    pub snap: Snapshot,
    pub hist: History,
    pub rows: Vec<Row>,
    pub desk: Desk,
    pub journal: Option<bool>,
    pub pending: Vec<Pending>,
    pub asking: HashMap<u32, Asked>,
    /// When each question appeared (`pacer` time): a yes counts only `ARM` later, since "Sofort beenden" opens by
    /// itself and a click meant for the table may already be on its way to where its button appears.
    pub asked_at: HashMap<u32, Duration>,
    /// The text bitscomp offers as the clipboard ("Bericht für Claude kopieren").
    pub clip: Option<String>,
    pub root: Root,
    pub own_uid: u32,
    open: Arc<AtomicBool>,
    want: Arc<Mutex<Want>>,
    wake: Option<mpsc::Sender<()>>,
    started: bool,
    frames_before: Option<(f64, crate::stats::Stats)>,
    pub samples: u64,
    /// Each window's kept frame (sys_draw).
    pub frames: HashMap<u32, Option<(crate::sys_draw::FrameKey, crate::canvas::Canvas)>>,
}

impl Default for Sys {
    fn default() -> Self {
        Sys {
            snap: Snapshot::default(),
            hist: History::default(),
            rows: Vec::new(),
            desk: Desk::default(),
            journal: None,
            pending: Vec::new(),
            asking: HashMap::new(),
            asked_at: HashMap::new(),
            clip: None,
            root: Root::system(),
            own_uid: crate::drop_root::euid(),
            open: Arc::new(AtomicBool::new(false)),
            want: Arc::new(Mutex::new(Want::default())),
            wake: None,
            started: false,
            frames_before: None,
            samples: 0,
            frames: HashMap::new(),
        }
    }
}

/// The journal's series for the page's: (page series, journal series, how many to add up).
const DAY: [(&str, &str); 4] = [("cpu", "cpu.all.busy"), ("temp.cpu", "temp.tctl"), ("net.rx", ".rx_bytes"), ("net.tx", ".tx_bytes")];

/// The last 24 h from `bitsjournal query` (288 points of 5 min); None when the journal is not readable.
fn journal_day(wall: i64) -> Option<Vec<(String, Vec<Option<f64>>)>> {
    let exe = ["/bin/bitsjournal", "/boot/BITSOS2/bin/bitsjournal"].into_iter().find(|p| std::path::Path::new(p).is_file())?;
    let out = std::process::Command::new(exe)
        .args(["query", "kind=metrics", "name=cpu.all.busy,temp.tctl,rx_bytes,tx_bytes", &format!("since={}", wall - 86_400), &format!("until={wall}"), "max=288"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut sums: HashMap<&str, Vec<Option<f64>>> = HashMap::new();
    for l in text.lines().skip(2) {
        let mut w = l.split_whitespace();
        let (Some(name), Some(_unit)) = (w.next(), w.next()) else { continue };
        let pts: Vec<Option<f64>> = w.map(|v| v.parse().ok()).collect();
        for (page, j) in DAY {
            let hit = if j.starts_with('.') { name.ends_with(j) } else { name == j };
            if hit {
                let e = sums.entry(page).or_insert_with(|| vec![None; pts.len()]);
                for (a, b) in e.iter_mut().zip(&pts) {
                    *a = match (*a, b) {
                        (Some(x), Some(y)) => Some(x + y),
                        (None, y) => *y,
                        (x, None) => x,
                    };
                }
            }
        }
    }
    (!sums.is_empty()).then(|| sums.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// A fixture's window list: `id pid hung x11 app_id title...` lines.
pub fn fixture_windows(text: &str) -> Vec<Win> {
    text.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            Some(Win {
                id: w.next()?.parse().ok()?,
                pid: w.next()?.parse().ok()?,
                hung: w.next()? == "1",
                x11: w.next()? == "1",
                app_id: w.next()?.to_string(),
                title: w.collect::<Vec<_>>().join(" "),
            })
        })
        .collect()
}

impl App {
    /// Starts the worker at bitscomp's start; it sleeps until a System window opens.
    pub fn sys_start(&mut self, handle: &LoopHandle<'static, App>) {
        if self.sys.started {
            return;
        }
        self.sys.started = true;
        let (tx, rx) = channel::channel::<Msg>();
        if let Err(e) = handle.insert_source(rx, |ev, _, app: &mut App| {
            if let Event::Msg(m) = ev {
                app.sys_msg(m);
            }
        }) {
            eprintln!("bitscomp: system: no channel ({e}); the program shows no values");
            return;
        }
        let (wtx, wrx) = mpsc::channel::<()>();
        let (open, want, root) = (self.sys.open.clone(), self.sys.want.clone(), self.sys.root.clone());
        let spawned = std::thread::Builder::new().name("bitscomp-system".into()).spawn(move || {
            let mut s = Sampler::new(root);
            let mut day_at: Option<f64> = None;
            loop {
                if !open.load(Ordering::Relaxed) {
                    if wrx.recv().is_err() {
                        return;
                    }
                    continue;
                }
                let w = *want.lock().unwrap_or_else(|e| e.into_inner());
                let now = crate::sys_sample::now_s();
                let snap = s.sample(w, now, crate::status::epoch_secs());
                if tx.send(Msg::Snap(Box::new(snap))).is_err() {
                    return;
                }
                if w.day && s.root.is_real() && day_at.is_none_or(|a| now - a >= 300.0) {
                    day_at = Some(now);
                    if tx.send(Msg::Day(journal_day(crate::status::epoch_secs()))).is_err() {
                        return;
                    }
                }
                match wrx.recv_timeout(Duration::from_secs(1)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        });
        match spawned {
            Ok(_) => self.sys.wake = Some(wtx),
            Err(e) => eprintln!("bitscomp: system: no thread ({e}); the program shows no values"),
        }
    }

    pub(crate) fn sys_pages(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.pages.by_pid.iter().filter(|(_, s)| s.kind == PageKind::System).map(|(p, _)| *p).collect();
        v.sort();
        v
    }

    /// What the shown tabs need; wakes the worker for at once when it changed.
    pub(crate) fn sys_want(&mut self) {
        let views: Vec<View> = self.sys_pages().iter().filter_map(|p| self.pages.by_pid.get(p).map(|s| s.sys.clone())).collect();
        let w = Want {
            procs: views.iter().any(|v| v.tab.needs_procs()),
            gpu_procs: views.iter().any(|v| v.tab == Tab::Procs),
            day: views.iter().any(|v| v.range == Range::Day),
        };
        // Only a System window Georg can see is sampled: not a minimised one, nor one on another workspace.
        let open = !views.is_empty() && self.sys_visible();
        let was = self.sys.open.swap(open, Ordering::Relaxed);
        let mut cur = self.sys.want.lock().unwrap_or_else(|e| e.into_inner());
        let changed = *cur != w;
        *cur = w;
        drop(cur);
        // A click that changes nothing the worker reads must not cost an extra walk over /proc.
        if (changed || (open && !was)) && let Some(t) = &self.sys.wake {
            let _ = t.send(());
        }
    }

    /// Whether a System window can be seen: on the shown workspace and not minimised (one not mapped yet counts,
    /// it is about to be).
    fn sys_visible(&self) -> bool {
        self.sys_pages().iter().any(|p| match self.sys_page_window(*p) {
            None => true,
            Some(w) => self.ws.all_windows().any(|x| x == &w) && !self.ws.minimized.iter().any(|m| m.win == w),
        })
    }

    /// A System window is drawn: it can be seen again (back from minimised or another workspace).
    fn sys_seen(&mut self) {
        if !self.sys.open.load(Ordering::Relaxed) {
            self.sys_want();
        }
    }

    /// A System window was started (page_spawned).
    pub(crate) fn sys_opened(&mut self) {
        self.sys_want();
    }

    /// After pages went away: the worker sleeps when no System window is left.
    pub fn sys_pages_gone(&mut self) {
        let left = self.sys_pages();
        self.sys.asking.retain(|p, _| left.contains(p));
        self.sys.asked_at.retain(|p, _| left.contains(p));
        self.sys.pending.retain(|e| left.contains(&e.page));
        self.sys.frames.retain(|p, _| left.contains(p));
        self.sys_want();
    }

    pub(crate) fn sys_windows(&self) -> Vec<Win> {
        // A fixture's processes are not this machine's: its windows come from its own list (host scenario).
        if !self.sys.root.is_real() {
            return self.sys.root.read("/windows").map(|t| fixture_windows(&t)).unwrap_or_default();
        }
        self.all_windows()
            .iter()
            .map(|w| {
                let (app_id, title) = crate::ctl::names(w);
                Win { id: crate::ctl::win_id(w), pid: u32::try_from(self.pid_of(w)).unwrap_or(0), app_id, title, hung: self.is_hung(w), x11: w.x11().is_some() }
            })
            .collect()
    }

    pub(crate) fn sys_regroup(&mut self) {
        let wins = self.sys_windows();
        let lib = self.library.clone();
        let name_of = move |id: &str| -> Option<String> {
            if id.is_empty() {
                return None;
            }
            if id == crate::sys_view::APP_ID {
                return Some(crate::sys_view::TITLE.into());
            }
            lib.entries.iter().find(|e| e.owns(id)).map(|e| e.name.clone())
        };
        let ctx = grp::Ctx { own_pid: std::process::id(), own_uid: self.sys.own_uid, name_of: &name_of };
        self.sys.rows = grp::group(&self.sys.snap.procs, &wins, &ctx);
        self.sys.desk.hung = wins.iter().filter(|w| w.hung).count();
    }

    /// The desktop's numbers of the last second from bitscomp's frame counters.
    fn sys_desk(&mut self, t: f64) {
        let now = self.stats.clone();
        let d = &mut self.sys.desk;
        if let Some((t0, before)) = self.sys.frames_before.take()
            && now.frames >= before.frames
            && t > t0
        {
            let n = (now.frames - before.frames) as f64;
            let per = |a: Duration, b: Duration| (n > 0.0).then(|| a.saturating_sub(b).as_secs_f64() * 1e3 / n);
            d.fps = Some(n / (t - t0));
            d.build_ms = per(now.build, before.build);
            d.render_ms = per(now.render, before.render);
            d.present_ms = per(now.present, before.present);
            let ev = now.input_events.saturating_sub(before.input_events);
            d.input_ms = (ev > 0).then(|| now.input_age_us.saturating_sub(before.input_age_us) as f64 / ev as f64 / 1e3);
        }
        d.frames = now.frames;
        d.size = self.output.current_mode().map(|m| (m.size.w, m.size.h));
        d.refresh_hz = (!self.refresh.is_zero()).then(|| 1.0 / self.refresh.as_secs_f64());
        self.sys.frames_before = Some((t, now));
    }

    fn sys_record(&mut self) {
        let s = &self.sys.snap;
        let t = s.t;
        let m = s.mem.mem;
        let sum = |f: &dyn Fn(&crate::sys_snap::Net) -> Option<f64>| s.nets.iter().map(f).fold(None, |a: Option<f64>, x| x.map(|x| a.unwrap_or(0.0) + x).or(a));
        let (dr, dw) = (
            s.disks.iter().filter_map(|d| d.read).fold(None, |a: Option<f64>, x| Some(a.unwrap_or(0.0) + x)),
            s.disks.iter().filter_map(|d| d.write).fold(None, |a: Option<f64>, x| Some(a.unwrap_or(0.0) + x)),
        );
        let d = &self.sys.desk;
        let frame = [d.build_ms, d.render_ms, d.present_ms].iter().flatten().copied().fold(None, |a: Option<f64>, x| Some(a.unwrap_or(0.0) + x));
        let vals: Vec<(&str, Option<f64>)> = vec![
            ("cpu", s.cpu.busy),
            ("cpu.programs", s.cpu.programs),
            ("cpu.kernel", s.cpu.kernel),
            ("mem.used", m.map(|m| m.used() as f64)),
            ("mem.cache", m.map(|m| m.cache() as f64)),
            ("gpu", s.gpu.busy),
            ("net.rx", sum(&|n| n.rx)),
            ("net.tx", sum(&|n| n.tx)),
            ("disk.read", dr),
            ("disk.write", dw),
            ("temp.cpu", s.heat.cpu),
            ("temp.gpu", s.heat.gpu),
            ("temp.nvme", s.heat.nvme),
            ("power.cpu", s.heat.cpu_power),
            ("power.gpu", s.heat.gpu_power),
            ("desk.frame_ms", frame),
            ("desk.fps", d.fps),
        ];
        for (n, v) in vals {
            self.sys.hist.add(t, n, v);
        }
    }

    fn sys_msg(&mut self, m: Msg) {
        match m {
            Msg::Snap(s) => {
                let t = s.t as f64;
                let procs_read = s.procs_read;
                let keep = std::mem::take(&mut self.sys.snap.procs);
                self.sys.snap = *s;
                if !procs_read {
                    // A tab without processes keeps the last list for Überblick's link and the detail.
                    self.sys.snap.procs = keep;
                }
                self.sys.samples += 1;
                self.sys_desk(crate::sys_sample::now_s().max(t));
                self.sys_record();
                if procs_read {
                    self.sys_regroup();
                }
                self.sys_pending_tick();
            }
            Msg::Day(d) => {
                self.sys.journal = Some(d.is_some());
                for (name, pts) in d.unwrap_or_default() {
                    self.sys.hist.set_day(&name, &pts);
                }
            }
        }
        // A window hidden since the last second stops the sampling (and so the frames it asks for).
        self.sys_want();
        if self.sys.open.load(Ordering::Relaxed) {
            self.needs_frame = true;
        }
    }

    pub(crate) fn sys_model(&self, v: &View) -> TabModel {
        let (_, off) = crate::werhatwas_app::clock(self.status.fixed);
        let c = model::Ctx {
            s: &self.sys.snap,
            h: &self.sys.hist,
            v,
            rows: &self.sys.rows,
            desk: &self.sys.desk,
            off_min: off,
            journal: self.sys.journal,
            own_uid: self.sys.own_uid,
            real_root: self.sys.root.is_real(),
        };
        model::build(&c)
    }

    pub(crate) fn sys_layout(&self, pid: u32, content: Rect) -> Option<(TabModel, crate::sys_layout::Layout)> {
        let s = self.pages.by_pid.get(&pid).filter(|s| s.kind == PageKind::System)?;
        let m = self.sys_model(&s.sys);
        let status = m.status.as_ref().map(|x| x.0.clone());
        let l = crate::sys_layout::layout(content, &m, s.sys.tab, status.as_deref(), s.scroll);
        Some((m, l))
    }

    pub(crate) fn sys_prepare(&mut self, pid: u32, content: Rect) {
        self.sys_pin(pid);
        self.sys_seen();
        let Some((_, l)) = self.sys_layout(pid, content) else { return };
        if let Some(s) = self.pages.by_pid.get_mut(&pid) {
            s.scroll = s.scroll.clamp(0, l.max_scroll());
        }
    }

    pub(crate) fn sys_look(&self, pid: u32, content: Rect) -> Option<SysLook> {
        let s = self.pages.by_pid.get(&pid)?;
        let (model, layout) = self.sys_layout(pid, content)?;
        let (hover, dhover) = match self.desk.hover {
            Some(Spot::Page(p, Part::Sys(x))) if p == pid => (Some(x), None),
            Some(Spot::Page(p, q @ (Part::DialogOk | Part::DialogCancel))) if p == pid => (None, Some(q)),
            _ => (None, None),
        };
        let field = s.edit.as_ref().map(|(p, e)| crate::page_draw::FieldLook { part: *p, text: e.text.clone(), before: e.before().to_string() });
        Some(SysLook {
            layout,
            model,
            theme: self.theme_gen,
            address: s.address.clone(),
            tab: s.sys.tab,
            range: s.sys.range,
            hover,
            dhover,
            field,
            msg: s.msg.clone(),
            dialog: s.confirm.clone(),
        })
    }

    /// `ctl hot` lines of a System window.
    pub(crate) fn sys_hot(&self, pid: u32, content: Rect, dialog: Option<&crate::page_confirm::Dialog>) -> String {
        let sc = self.theme.scale;
        let at = |r: Rect| (((r.x + r.w / 2) as f64 * sc).round(), ((r.y + r.h / 2) as f64 * sc).round());
        let mut out = String::new();
        if let Some(d) = dialog {
            for (part, r) in crate::page_confirm::parts(content, d) {
                let (x, y) = at(r);
                out.push_str(&format!("page pid={pid} {} at={x},{y}\n", crate::page::part_name(&[], part)));
            }
            return out;
        }
        let Some((_, l)) = self.sys_layout(pid, content) else { return out };
        for (p, r) in &l.parts {
            let (x, y) = at(*r);
            out.push_str(&format!("page pid={pid} system={} at={x},{y}\n", crate::sys_view::part_name(*p)));
        }
        out
    }

    /// The page's tab changes (a click, `ctl system tab`, Strg+Umschalt+Esc): its address follows.
    pub(crate) fn sys_set_tab(&mut self, pid: u32, t: Tab) {
        if let Some(s) = self.pages.by_pid.get_mut(&pid) {
            if s.sys.tab != t {
                s.sys.tab = t;
                s.scroll = 0;
                s.address = crate::sys_view::address(t);
                s.msg = None;
            }
            self.needs_frame = true;
        }
        self.sys_want();
    }

    /// Strg+Umschalt+Esc: the task manager, in the System window there is, else in a new one.
    pub fn sys_shortcut(&mut self) {
        let pages = self.sys_pages();
        let focused = self.ws.focused().and_then(|w| self.page_pid(w)).filter(|p| pages.contains(p));
        let pid = focused.or_else(|| pages.iter().copied().find(|p| self.sys_page_window(*p).is_some()));
        match pid {
            Some(p) => {
                self.sys_set_tab(p, Tab::Procs);
                if let Some(w) = self.sys_page_window(p) {
                    self.sys_show_window(&w);
                }
            }
            None => self.open_page(&crate::sys_view::address(Tab::Procs), crate::address::Where::Tab, None),
        }
    }

    pub(crate) fn sys_page_window(&self, pid: u32) -> Option<crate::win::Window> {
        self.all_windows().into_iter().find(|w| self.page_pid(w) == Some(pid))
    }

    /// Brings a window to the front: its workspace, out of the minimised ones, focused.
    pub(crate) fn sys_show_window(&mut self, w: &crate::win::Window) {
        if !self.ws.all_windows().any(|x| x == w)
            && let Some(i) = self.desk.parked.iter().position(|ws| ws.all_windows().any(|x| x == w))
        {
            self.ws_switch(i);
        }
        if self.ws.minimized.iter().any(|m| &m.win == w) {
            self.unminimize(w, "system");
        }
        self.ws.focus_window(w);
        self.layout_dirty = true;
        self.needs_frame = true;
    }
}
