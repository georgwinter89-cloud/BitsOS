//! The seven actions. Answers are one JSON object as text (machine-readable for Claude and the scenarios);
//! screenshot_tab adds the PNG. Every wait is bounded by the call's deadline and its cancel flag.

use crate::browser::{Browser, Got, Want};
use crate::cdp::Conn;
use crate::js;
use bitsact::events::{Events, Level, q};
use bitsact::{Action, Content, Ctx, Handler, Json, Reply, Safety};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const SERVICE: &str = "brave";
/// The largest PNG answered as it is: base64 makes it 4/3 larger, and an answer has at most 4 MiB (MC-6).
const PNG_MAX: usize = 2900 << 10;
/// read_page's max_chars at most (characters, cut in the page). The answer itself is cut again by its encoded
/// bytes (ANSWER_BUDGET): a character can take up to 6 bytes in the JSON text (\u escapes), 3-4 as UTF-8.
const TEXT_MAX: u64 = 1_000_000;
/// The largest answer read_page builds; the kernel takes at most 4 MiB (MC-6), the rest is margin.
const ANSWER_BUDGET: usize = 3 << 20;
/// Where downloads may go besides brave-act's download folder: the home of uid 1000 (Brave's user).
const HOME_1000: &str = "/data/home/1000";

const TAB: &str = r#""tab":{"type":"string","description":"tab id from brave_tabs (default: the visible tab)"}"#;

fn schema(props: &str, required: &[&str], timeout_ms: u64) -> String {
    let req: Vec<String> = required.iter().map(|r| format!("\"{r}\"")).collect();
    let sep = if props.is_empty() { "" } else { "," };
    format!(
        r#"{{"type":"object","properties":{{{props}{sep}"timeout_ms":{{"type":"integer","minimum":1,"default":{timeout_ms}}}}},"required":[{}]}}"#,
        req.join(",")
    )
}

pub fn list() -> Vec<Action> {
    let start = " Starts Brave (through its launcher, as uid 1000, in the desktop) when brave-act has none running.";
    vec![
        Action::new(
            "brave_open_url",
            &format!("Open a URL (http, https or about:blank; no file: URLs) in Brave and wait until it has loaded; answers tab id, title and final URL. new_tab (default true) opens a tab, else the tab `tab` (or the visible one) navigates.{start} With a Brave that was started without brave-act, the URL is handed to it (other actions need a Brave started by brave-act)."),
            &schema(&format!(r#""url":{{"type":"string"}},"new_tab":{{"type":"boolean"}},{TAB}"#), &["url"], 60_000),
            Safety::Read,
        ),
        Action::new(
            "brave_tabs",
            &format!("List Brave's tabs: id, title, URL.{start}"),
            &schema("", &[], 30_000),
            Safety::Read,
        ),
        Action::new(
            "brave_read_page",
            "The visible text of a tab (innerText), with its title and URL, and its links ([text, href], the first 200) unless links is false; max_chars cuts the text (default 100000); a text that does not fit an answer (about 3 MB) is cut too (truncated true).",
            &schema(&format!(r#"{TAB},"max_chars":{{"type":"integer","minimum":1,"maximum":{TEXT_MAX}}},"links":{{"type":"boolean"}}"#), &[], 30_000),
            Safety::Read,
        ),
        Action::new(
            "brave_screenshot_tab",
            "A PNG of what a tab shows (its viewport). The tab is brought to the front first (a hidden tab draws nothing). A picture over 2.9 MB comes at half the size.",
            &schema(TAB, &[], 30_000),
            Safety::Read,
        ),
        Action::new(
            "brave_click_link",
            "Follow a link of a tab: the index-th (default 0) <a href> whose text contains `text` (case-insensitive) and/or whose href is or contains `href`; the tab goes to the link's URL (http or https) and the answer has the new title and URL once it loaded. Page scripts of the link (onclick) are not run.",
            &schema(&format!(r#"{TAB},"text":{{"type":"string"}},"href":{{"type":"string"}},"index":{{"type":"integer","minimum":0}}"#), &[], 60_000),
            Safety::Read,
        ),
        Action::new(
            "brave_fill_form",
            "Fill form fields of a tab and optionally submit the form. fields: {key: value}, key = the field's name, id or a CSS selector; value text, or true/false for a checkbox, the value of a radio button, an option's value or text for a select. All fields are checked before any is set. Password, card and file fields are refused. submit (default false) submits the first field's form and waits for the next page. Needs Georg's permission (setting claude.actions.brave_fill_form = allow).",
            &schema(&format!(r#"{TAB},"fields":{{"type":"object"}},"submit":{{"type":"boolean"}}"#), &["fields"], 60_000),
            Safety::Change,
        )
        .gate_text("Claude darf in Brave Formulare ausfüllen und absenden", "Claude may fill in and submit forms in Brave"),
        Action::new(
            "brave_download",
            "Download a URL with Brave (its cookies and session) into a folder (default /data/home/1000/Downloads, or /tmp/Downloads without the data volume; `dir` is a folder inside it, or inside /data/home/1000; links are not followed); answers the file's path and size once complete. A URL that shows a page instead of downloading is an error. Needs Georg's permission (setting claude.actions.brave_download = allow).",
            &schema(r#""url":{"type":"string"},"dir":{"type":"string","description":"folder for the file: relative to the download folder, or absolute inside it or inside /data/home/1000"}"#, &["url"], 300_000),
            Safety::Change,
        )
        .gate_text("Claude darf mit Brave Dateien herunterladen", "Claude may download files with Brave"),
    ]
}

pub struct Brave {
    pub browser: Arc<Browser>,
    pub download_dir: PathBuf,
    pub ev: Events,
    /// One download at a time: the download behaviour is the browser's, not a tab's. Waited for with try_lock in
    /// steps, so a waiting call keeps its deadline and cancel flag (B4).
    dl: Mutex<()>,
}

impl Brave {
    pub fn new(browser: Arc<Browser>, download_dir: PathBuf, ev: Events) -> Brave {
        Brave { browser, download_dir, ev, dl: Mutex::new(()) }
    }
}

impl Handler for Brave {
    fn call(&self, ctx: &Ctx) -> Reply {
        let r = match ctx.call.name.as_str() {
            "brave_open_url" => self.open_url(ctx),
            "brave_tabs" => self.tabs(ctx),
            "brave_read_page" => self.read_page(ctx),
            "brave_screenshot_tab" => self.screenshot(ctx),
            "brave_click_link" => self.click_link(ctx),
            "brave_fill_form" => self.fill_form(ctx),
            "brave_download" => self.download(ctx),
            n => Err(Reply::error(format!("{n}: unknown action"))),
        };
        r.unwrap_or_else(|e| e)
    }
}

type R<T> = Result<T, Reply>;

fn err(e: String) -> Reply {
    Reply::error(e)
}

fn answer(j: Json) -> Reply {
    Reply::text(j.to_string())
}

/// http, https, about:blank. No file: read actions would read any file Brave's user can (read_page,
/// screenshot_tab) without the permission the change actions need.
fn allowed_url(u: &str) -> bool {
    let l = u.trim().to_ascii_lowercase();
    ["http://", "https://"].iter().any(|p| l.starts_with(p) && l.len() > p.len()) || l == "about:blank"
}

fn bad_url(u: &str) -> Reply {
    Reply::bad_argument(format!("url: {u:?} is not an http or https URL (or about:blank)"))
}

/// A tab attached for this call; detached when dropped.
struct Tab<'a> {
    conn: &'a Conn,
    id: String,
    session: String,
}

impl Drop for Tab<'_> {
    fn drop(&mut self) {
        let p = Json::obj(vec![("sessionId", Json::str(&self.session))]);
        let _ = self.conn.call("Target.detachFromTarget", p, None, Instant::now() + Duration::from_secs(2));
    }
}

impl Tab<'_> {
    fn call(&self, method: &str, params: Json, ctx: &Ctx) -> R<Json> {
        self.conn.call(method, params, Some(&self.session), ctx.deadline()).map_err(err)
    }

    /// The value of `expr` (a promise is awaited).
    fn eval(&self, expr: &str, ctx: &Ctx) -> R<Json> {
        let p = Json::obj(vec![("expression", Json::str(expr)), ("returnByValue", Json::Bool(true)), ("awaitPromise", Json::Bool(true))]);
        let r = self.call("Runtime.evaluate", p, ctx)?;
        if let Some(x) = r.get("exceptionDetails") {
            let what = x.get("exception").and_then(|e| e.get("description")).and_then(Json::as_str).or_else(|| x.get("text").and_then(Json::as_str)).unwrap_or("an exception");
            return Err(Reply::error(format!("the page's script failed: {what}")));
        }
        Ok(r.get("result").and_then(|v| v.get("value")).cloned().unwrap_or(Json::Null))
    }

    /// Waits until `cond` (a JS expression) is true; a navigation in between (context gone) is retried.
    fn wait_for(&self, cond: &str, ctx: &Ctx, what: &str, limit: Option<Instant>) -> R<bool> {
        loop {
            ctx.alive(what)?;
            let p = Json::obj(vec![("expression", Json::str(cond)), ("returnByValue", Json::Bool(true))]);
            let short = (Instant::now() + Duration::from_secs(5)).min(ctx.deadline());
            if let Ok(r) = self.conn.call("Runtime.evaluate", p, Some(&self.session), short)
                && r.get("result").and_then(|v| v.get("value")).and_then(Json::as_bool) == Some(true)
            {
                return Ok(true);
            }
            if limit.is_some_and(|l| Instant::now() >= l) {
                return Ok(false);
            }
            if !self.conn.alive() {
                return Err(Reply::error(format!("{what}: {}", self.conn.why())));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn navigate(&self, url: &str, ctx: &Ctx) -> R<()> {
        // A link's page may redirect anywhere; Chromium itself does not let http(s) go to file:.
        let r = self.call("Page.navigate", Json::obj(vec![("url", Json::str(url))]), ctx)?;
        if let Some(e) = r.get("errorText").and_then(Json::as_str).filter(|e| !e.is_empty()) {
            return Err(Reply::error(format!("{url}: {e}")));
        }
        self.wait_for("document.readyState === 'complete'", ctx, "waiting for the page to load", None).map(|_| ())
    }

    fn info(&self, ctx: &Ctx) -> R<Json> {
        let v = self.eval("({title: document.title, url: location.href})", ctx)?;
        Ok(Json::obj(vec![("tab", Json::str(&self.id)), ("title", v.get("title").cloned().unwrap_or(Json::Null)), ("url", v.get("url").cloned().unwrap_or(Json::Null))]))
    }
}

fn pages(conn: &Conn, ctx: &Ctx) -> R<Vec<(String, String, String)>> {
    let r = conn.call("Target.getTargets", Json::Obj(Vec::new()), None, ctx.deadline()).map_err(err)?;
    let s = |t: &Json, k: &str| t.get(k).and_then(Json::as_str).unwrap_or("").to_string();
    Ok(r.get("targetInfos")
        .and_then(Json::as_array)
        .unwrap_or(&[])
        .iter()
        .filter(|t| t.get("type").and_then(Json::as_str) == Some("page"))
        .map(|t| (s(t, "targetId"), s(t, "title"), s(t, "url")))
        .collect())
}

fn attach<'a>(conn: &'a Conn, id: &str, ctx: &Ctx) -> R<Tab<'a>> {
    let p = Json::obj(vec![("targetId", Json::str(id)), ("flatten", Json::Bool(true))]);
    let r = conn.call("Target.attachToTarget", p, None, ctx.deadline()).map_err(err)?;
    let session = r.get("sessionId").and_then(Json::as_str).ok_or_else(|| Reply::error("Target.attachToTarget gave no sessionId"))?;
    Ok(Tab { conn, id: id.to_string(), session: session.to_string() })
}

/// The tab `want` (its id or a unique start of it), else the visible tab (else the first).
fn pick<'a>(conn: &'a Conn, want: Option<&str>, ctx: &Ctx) -> R<Tab<'a>> {
    let all = pages(conn, ctx)?;
    if all.is_empty() {
        return Err(Reply::error("Brave has no tab open"));
    }
    if let Some(w) = want {
        let m: Vec<_> = all.iter().filter(|(id, _, _)| id == w || (w.len() >= 4 && id.starts_with(w))).collect();
        return match m.as_slice() {
            [one] => attach(conn, &one.0, ctx),
            [] => Err(Reply::bad_argument(format!("tab: no tab {w:?} (brave_tabs lists them)"))),
            _ => Err(Reply::bad_argument(format!("tab: {w:?} fits {} tabs; give more of the id", m.len()))),
        };
    }
    let mut first = None;
    for (id, _, _) in &all {
        let t = attach(conn, id, ctx)?;
        // A tab in the middle of a navigation has no context to evaluate in: not the visible one for now.
        if t.eval("document.visibilityState", ctx).ok().and_then(|v| v.as_str().map(String::from)).as_deref() == Some("visible") {
            return Ok(t);
        }
        if first.is_none() {
            first = Some(id.clone());
        }
    }
    attach(conn, first.as_deref().unwrap_or(&all[0].0), ctx)
}

/// The tab that shows (or loads) `url`, waited for a few seconds; else the visible one.
fn first_with<'a>(conn: &'a Conn, url: &str, ctx: &Ctx) -> R<Tab<'a>> {
    let same = |u: &str| u == url || u.trim_end_matches('/') == url.trim_end_matches('/');
    let until = (Instant::now() + Duration::from_secs(10)).min(ctx.deadline());
    while Instant::now() < until {
        if let Some((id, _, _)) = pages(conn, ctx)?.into_iter().find(|(_, _, u)| same(u)) {
            return attach(conn, &id, ctx);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    pick(conn, None, ctx)
}

fn call_js(f: &str, args: &[Json]) -> String {
    let a: Vec<String> = args.iter().map(|j| j.to_string()).collect();
    format!("({f})({})", a.join(","))
}

impl Brave {
    /// Brave started by brave-act, or the answer why there is none.
    fn conn(&self, ctx: &Ctx, want: Want<'_>) -> R<(Arc<Conn>, bool)> {
        match self.browser.get(want, ctx.deadline()).map_err(err)? {
            Got::Up { conn, started_now } => Ok((conn, started_now)),
            Got::Foreign(pid) => Err(Reply::error(format!(
                "Brave runs (pid {pid}) but was started without brave-act, so it has no DevTools pipe: only brave_open_url \
works with it. Close that Brave (or let Georg close it); the next call starts one brave-act can drive."
            ))),
        }
    }

    fn open_url(&self, ctx: &Ctx) -> R<Reply> {
        let a = ctx.args();
        let url = a.str("url")?;
        if !allowed_url(url) {
            return Err(bad_url(url));
        }
        let new_tab = a.bool_or("new_tab", true)?;
        let tab = a.opt_str("tab")?;
        let got = self.browser.get(Want::Url(url), ctx.deadline()).map_err(err)?;
        let (conn, started_now) = match got {
            Got::Up { conn, started_now } => (conn, started_now),
            Got::Foreign(pid) => {
                let said = self.browser.forward(url, ctx.deadline()).map_err(err)?;
                self.ev.emit(Level::Info, "open_url.forwarded", &format!("pid={pid}"));
                return Ok(answer(Json::obj(vec![
                    ("handed_to", Json::Num(f64::from(pid))),
                    ("url", Json::str(url)),
                    ("note", Json::str("Brave was started without brave-act: the URL went to it; tabs, read_page and the other actions need a Brave started by brave-act")),
                    ("launcher", Json::str(&said)),
                ])));
            }
        };
        let t = if started_now {
            // Brave's first tab has the URL already (after a crash Brave may show other tabs too: the one with it).
            let t = first_with(&conn, url, ctx)?;
            t.wait_for("document.readyState === 'complete' && location.href !== 'about:blank'", ctx, "waiting for the page to load", None)?;
            t
        } else if new_tab {
            let p = Json::obj(vec![("url", Json::str("about:blank"))]);
            let r = conn.call("Target.createTarget", p, None, ctx.deadline()).map_err(err)?;
            let id = r.get("targetId").and_then(Json::as_str).ok_or_else(|| Reply::error("Target.createTarget gave no targetId"))?;
            let t = attach(&conn, id, ctx)?;
            t.navigate(url, ctx)?;
            t
        } else {
            let t = pick(&conn, tab, ctx)?;
            t.navigate(url, ctx)?;
            t
        };
        let _ = conn.call("Target.activateTarget", Json::obj(vec![("targetId", Json::str(&t.id))]), None, ctx.deadline());
        Ok(answer(t.info(ctx)?))
    }

    fn tabs(&self, ctx: &Ctx) -> R<Reply> {
        let (conn, _) = self.conn(ctx, Want::Plain)?;
        let list = pages(&conn, ctx)?
            .into_iter()
            .map(|(id, title, url)| Json::obj(vec![("tab", Json::Str(id)), ("title", Json::Str(title)), ("url", Json::Str(url))]))
            .collect();
        Ok(answer(Json::obj(vec![("tabs", Json::Arr(list))])))
    }

    fn read_page(&self, ctx: &Ctx) -> R<Reply> {
        let a = ctx.args();
        let max = a.u64_in("max_chars", 100_000, 1, TEXT_MAX)?;
        let links = a.bool_or("links", true)?;
        let (conn, _) = self.conn(ctx, Want::Plain)?;
        let t = pick(&conn, a.opt_str("tab")?, ctx)?;
        let v = t.eval(&call_js(js::READ, &[Json::Num(max as f64), Json::Bool(links)]), ctx)?;
        let mut out = vec![("tab", Json::str(&t.id))];
        for k in ["title", "url", "length", "linkCount", "links"] {
            if let Some(x) = v.get(k) {
                out.push((k, x.clone()));
            }
        }
        let text = v.get("text").and_then(Json::as_str).unwrap_or("");
        let rest = Json::obj(out.clone()).to_string().len() + 64;
        let (text, cut) = fit_text(text, ANSWER_BUDGET.saturating_sub(rest));
        if cut {
            self.ev.emit(Level::Info, "read_page.truncated", &format!("chars={}", text.chars().count()));
        }
        out.push(("text", Json::Str(text)));
        out.push(("truncated", Json::Bool(cut || v.get("length").and_then(Json::as_u64).is_some_and(|l| l > max))));
        Ok(answer(Json::obj(out)))
    }

    fn screenshot(&self, ctx: &Ctx) -> R<Reply> {
        let (conn, _) = self.conn(ctx, Want::Plain)?;
        let t = pick(&conn, ctx.args().opt_str("tab")?, ctx)?;
        let _ = conn.call("Target.activateTarget", Json::obj(vec![("targetId", Json::str(&t.id))]), None, ctx.deadline());
        let _ = t.call("Page.bringToFront", Json::Obj(Vec::new()), ctx);
        let mut png = shot(&t, ctx, None)?;
        let mut scale = 1.0;
        if png.len() > PNG_MAX {
            let v = t.eval("({w: innerWidth, h: innerHeight})", ctx)?;
            let (w, h) = (v.get("w").and_then(Json::as_f64).unwrap_or(1280.0), v.get("h").and_then(Json::as_f64).unwrap_or(800.0));
            scale = 0.5;
            png = shot(&t, ctx, Some((w, h, scale)))?;
            if png.len() > PNG_MAX {
                return Err(Reply::error(format!("the tab's picture is {} KB even at half size; too large for an answer", png.len() >> 10)));
            }
        }
        let (w, h) = png_size(&png).unwrap_or((0, 0));
        let mut info = t.info(ctx)?;
        if let Json::Obj(m) = &mut info {
            m.push(("width".into(), Json::Num(f64::from(w))));
            m.push(("height".into(), Json::Num(f64::from(h))));
            m.push(("scale".into(), Json::Num(scale)));
        }
        Ok(Reply::Ok(vec![Content::Text(info.to_string()), Content::Png(png)]))
    }

    fn click_link(&self, ctx: &Ctx) -> R<Reply> {
        let a = ctx.args();
        let text = a.opt_str("text")?;
        let href = a.opt_str("href")?;
        if text.is_none() && href.is_none() {
            return Err(Reply::bad_argument("text or href: give at least one"));
        }
        let index = a.u64_in("index", 0, 0, 1 << 20)?;
        let (conn, _) = self.conn(ctx, Want::Plain)?;
        let t = pick(&conn, a.opt_str("tab")?, ctx)?;
        let s = |o: Option<&str>| o.map(Json::str).unwrap_or(Json::Null);
        let v = t.eval(&call_js(js::FIND_LINK, &[s(text), s(href), Json::Num(index as f64)]), ctx)?;
        let count = v.get("count").and_then(Json::as_u64).unwrap_or(0);
        let Some(to) = v.get("href").and_then(Json::as_str) else {
            return Err(Reply::error(format!("no link fits (text {text:?}, href {href:?}, index {index}; {count} fit)")));
        };
        if !allowed_url(to) {
            return Err(Reply::error(format!("the link goes to {to:?}: only http and https links are followed")));
        }
        let to = to.to_string();
        t.navigate(&to, ctx)?;
        let mut info = t.info(ctx)?;
        if let Json::Obj(m) = &mut info {
            m.push(("link".into(), Json::Str(to)));
            m.push(("matches".into(), Json::Num(count as f64)));
        }
        Ok(answer(info))
    }

    fn fill_form(&self, ctx: &Ctx) -> R<Reply> {
        let a = ctx.args();
        let fields = a.members("fields")?.unwrap_or(&[]);
        if fields.is_empty() {
            return Err(Reply::bad_argument("fields: want at least one {key: value}"));
        }
        if let Some((k, _)) = fields.iter().find(|(_, v)| !matches!(v, Json::Str(_) | Json::Bool(_) | Json::Num(_))) {
            return Err(Reply::bad_argument(format!("fields.{k}: want text, a number or true/false")));
        }
        let submit = a.bool_or("submit", false)?;
        let (conn, _) = self.conn(ctx, Want::Plain)?;
        let t = pick(&conn, a.opt_str("tab")?, ctx)?;
        let pairs = Json::Arr(fields.iter().map(|(k, v)| Json::Arr(vec![Json::str(k), v.clone()])).collect());
        let v = t.eval(&call_js(js::FILL, &[pairs, Json::Bool(submit)]), ctx)?;
        if let Some(e) = v.get("error").and_then(Json::as_str) {
            return Err(Reply::error(format!("nothing submitted: {e}")));
        }
        let mut out = vec![("tab", Json::str(&t.id)), ("set", v.get("set").cloned().unwrap_or(Json::Arr(Vec::new())))];
        if submit {
            let limit = (Instant::now() + Duration::from_secs(20)).min(ctx.deadline());
            let moved = t.wait_for(js::NEW_DOC_LOADED, ctx, "waiting for the page after the submit", Some(limit))?;
            out.push(("submitted", Json::Bool(true)));
            out.push(("new_page", Json::Bool(moved)));
        } else {
            out.push(("submitted", Json::Bool(false)));
        }
        let info = t.info(ctx)?;
        out.push(("title", info.get("title").cloned().unwrap_or(Json::Null)));
        out.push(("url", info.get("url").cloned().unwrap_or(Json::Null)));
        Ok(answer(Json::obj(out)))
    }

    fn download(&self, ctx: &Ctx) -> R<Reply> {
        let a = ctx.args();
        let url = a.str("url")?;
        if !allowed_url(url) || url.eq_ignore_ascii_case("about:blank") {
            return Err(bad_url(url));
        }
        let (root, dir) = download_dir(&self.download_dir, a.opt_str("dir")?)?;
        make_dir(&root, &dir, root == self.download_dir).map_err(|e| Reply::error(format!("{}: {e}", dir.display())))?;
        let (conn, _) = self.conn(ctx, Want::Plain)?;
        let _one = loop {
            match self.dl.try_lock() {
                Ok(g) => break g,
                // The guard keeps nothing: a panicked holder leaves no broken state behind.
                Err(std::sync::TryLockError::Poisoned(p)) => break p.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    ctx.alive("waiting for another brave_download to finish")?;
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        };
        // The caller may have given up while it waited: nothing is changed then.
        ctx.alive("the download")?;
        let set = |behavior: &str, d: Option<&Path>| {
            let mut p = vec![("behavior", Json::str(behavior)), ("eventsEnabled", Json::Bool(true))];
            if let Some(d) = d {
                p.push(("downloadPath", Json::Str(d.display().to_string())));
            }
            conn.call("Browser.setDownloadBehavior", Json::obj(p), None, Instant::now() + Duration::from_secs(5))
        };
        set("allowAndName", Some(&dir)).map_err(err)?;
        let r = self.download_in(&conn, url, &dir, ctx);
        // Brave's own downloads (Georg's) go back to his settings, whatever happened.
        if let Err(e) = set("default", None) {
            self.ev.emit(Level::Warn, "download.reset_failed", &format!("why={}", q(&e)));
        }
        r
    }

    fn download_in(&self, conn: &Conn, url: &str, dir: &Path, ctx: &Ctx) -> R<Reply> {
        let events = conn.subscribe(|j| matches!(j.get("method").and_then(Json::as_str), Some("Browser.downloadWillBegin" | "Browser.downloadProgress")));
        let r = conn.call("Target.createTarget", Json::obj(vec![("url", Json::str("about:blank")), ("background", Json::Bool(true))]), None, ctx.deadline()).map_err(err)?;
        let id = r.get("targetId").and_then(Json::as_str).ok_or_else(|| Reply::error("Target.createTarget gave no targetId"))?.to_string();
        let close = || {
            let _ = conn.call("Target.closeTarget", Json::obj(vec![("targetId", Json::str(&id))]), None, Instant::now() + Duration::from_secs(5));
        };
        let mut begun: Option<(String, String)> = None;
        // Downloads of other tabs (Georg's, or a page's script) while allowAndName holds: not this call's answer,
        // but saved under their own names too, so no guid-named file stays behind.
        let mut others: Vec<(String, String)> = Vec::new();
        let res = (|| {
            let t = attach(conn, &id, ctx)?;
            let nav = t.call("Page.navigate", Json::obj(vec![("url", Json::str(url))]), ctx)?;
            let nav_err = nav.get("errorText").and_then(Json::as_str).unwrap_or("").to_string();
            // The helper tab's main frame (its id is the target's in Chromium when Page.navigate gives none).
            let frame = nav.get("frameId").and_then(Json::as_str).unwrap_or(&id).to_string();
            let mut last_note = Instant::now();
            // A download aborts the navigation (errorText net::ERR_ABORTED) and begins soon; a URL whose page was
            // committed is a page, unless a download still begins within a few seconds (a script's).
            let wait = if nav_err.is_empty() { 3 } else { 30 };
            let begin_by = (Instant::now() + Duration::from_secs(wait)).min(ctx.deadline());
            loop {
                ctx.alive("the download")?;
                let Some(e) = events.next((Instant::now() + Duration::from_millis(500)).min(ctx.deadline())) else {
                    if begun.is_none() && Instant::now() >= begin_by {
                        let why = if nav_err.is_empty() { "it shows a page".to_string() } else { nav_err.clone() };
                        return Err(Reply::error(format!("{url} did not start a download ({why})")));
                    }
                    if !conn.alive() {
                        return Err(Reply::error(conn.why()));
                    }
                    continue;
                };
                let p = e.get("params").cloned().unwrap_or(Json::Null);
                let s = |k: &str| p.get(k).and_then(Json::as_str).unwrap_or("").to_string();
                match e.get("method").and_then(Json::as_str) {
                    Some("Browser.downloadWillBegin") if begun.is_none() && (s("frameId") == frame || s("url") == url) => {
                        self.ev.emit(Level::Info, "download.begin", &format!("url={} name={}", q(&s("url")), q(&s("suggestedFilename"))));
                        begun = Some((s("guid"), s("suggestedFilename")));
                    }
                    Some("Browser.downloadWillBegin") => {
                        self.ev.emit(Level::Info, "download.other", &format!("url={} frame={}", q(&s("url")), q(&s("frameId"))));
                        others.push((s("guid"), s("suggestedFilename")));
                    }
                    Some("Browser.downloadProgress") if s("state") == "completed" && others.iter().any(|(g, _)| *g == s("guid")) => {
                        let i = others.iter().position(|(g, _)| *g == s("guid")).unwrap_or(0);
                        let (guid, name) = others.swap_remove(i);
                        match place(dir, &guid, &name, &|w| ctx.alive(w)) {
                            Ok(p) => self.ev.emit(Level::Info, "download.other_saved", &format!("path={}", q(&p.display().to_string()))),
                            Err(_) => self.ev.emit(Level::Warn, "download.other_unsaved", &format!("guid={}", q(&guid))),
                        }
                    }
                    Some("Browser.downloadProgress") if begun.as_ref().is_some_and(|(g, _)| *g == s("guid")) => {
                        let got = p.get("receivedBytes").and_then(Json::as_f64).unwrap_or(0.0) as u64;
                        match s("state").as_str() {
                            "completed" => {
                                let (guid, name) = begun.take().unwrap_or_default();
                                let path = place(dir, &guid, &name, &|w| ctx.alive(w))?;
                                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(got);
                                self.ev.emit(Level::Info, "download.done", &format!("path={} bytes={size}", q(&path.display().to_string())));
                                return Ok(answer(Json::obj(vec![("path", Json::Str(path.display().to_string())), ("bytes", Json::Num(size as f64)), ("url", Json::str(url))])));
                            }
                            "canceled" => return Err(Reply::error(format!("Brave cancelled the download of {url}"))),
                            _ if last_note.elapsed() > Duration::from_secs(2) => {
                                ctx.progress(&format!("{got} bytes"));
                                last_note = Instant::now();
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        })();
        if res.is_err()
            && let Some((guid, _)) = &begun
        {
            // A download that did not finish in time stops: no half file stays behind under a guid name.
            let _ = conn.call("Browser.cancelDownload", Json::obj(vec![("guid", Json::str(guid))]), None, Instant::now() + Duration::from_secs(5));
            let _ = std::fs::remove_file(dir.join(guid));
        }
        close();
        res
    }
}

/// The finished file `dir/guid` under its suggested name (made safe, unique with " (n)"). Never replaces a file:
/// link(2) fails with EEXIST where rename(2) would replace (another download, a link planted there).
fn place(dir: &Path, guid: &str, name: &str, alive: &dyn Fn(&str) -> R<()>) -> R<PathBuf> {
    let from = dir.join(guid);
    while !from.exists() {
        alive("waiting for the downloaded file")?;
        std::thread::sleep(Duration::from_millis(100));
    }
    let base = safe_name(name);
    let (stem, ext) = match base.rfind('.') {
        Some(i) if i > 0 => (&base[..i], &base[i..]),
        _ => (&base[..], ""),
    };
    let mut n = 0u64;
    loop {
        alive("naming the downloaded file")?;
        let to = if n == 0 { dir.join(&base) } else { dir.join(format!("{stem} ({n}){ext}")) };
        n += 1;
        match std::fs::hard_link(&from, &to) {
            Ok(()) => {
                let _ = std::fs::remove_file(&from);
                return Ok(to);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            // A file system without hard links: rename where nothing (not even a dangling link) is.
            Err(_) if std::fs::symlink_metadata(&to).is_err() => {
                std::fs::rename(&from, &to).map_err(|e| Reply::error(format!("{} -> {}: {e}", from.display(), to.display())))?;
                return Ok(to);
            }
            Err(_) => continue,
        }
    }
}

/// The text cut to at most `budget` bytes of JSON text (on a character boundary); true when cut.
pub fn fit_text(text: &str, budget: usize) -> (String, bool) {
    let enc = |t: &str| Json::str(t).to_string().len();
    if enc(text) <= budget {
        return (text.to_string(), false);
    }
    let mut end = text.len().min(budget);
    loop {
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        let e = enc(&text[..end]);
        if end == 0 || e <= budget {
            return (text[..end].to_string(), true);
        }
        // Shrink by the share the escapes take, at least one byte (a few rounds).
        end = (end - 1).min((end as u128 * budget as u128 / e as u128) as usize);
    }
}

/// The download folder of a call and the root it lies in: `dir` relative to brave-act's download folder, or
/// absolute inside it or inside HOME_1000; never with `..`.
fn download_dir(default: &Path, dir: Option<&str>) -> R<(PathBuf, PathBuf)> {
    let Some(d) = dir else { return Ok((default.to_path_buf(), default.to_path_buf())) };
    let p = Path::new(d);
    if d.is_empty() || p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(Reply::bad_argument(format!("dir: {d:?} is not a folder without ..")));
    }
    let p = if p.is_absolute() { p.to_path_buf() } else { default.join(p) };
    for root in [default, Path::new(HOME_1000)] {
        if p.starts_with(root) {
            return Ok((root.to_path_buf(), p));
        }
    }
    Err(Reply::bad_argument(format!("dir: {d:?} is outside {} and {HOME_1000}", default.display())))
}

pub fn safe_name(name: &str) -> String {
    let n: String = name.rsplit('/').next().unwrap_or("").chars().map(|c| if c.is_control() || c == '\\' { '_' } else { c }).collect();
    let n = n.trim().trim_start_matches('.').to_string();
    let n: String = n.chars().take(200).collect();
    if n.is_empty() { "download".into() } else { n }
}

/// Makes the download folder `dir` below `root`. As root, only folders made here go to uid 1000 (Brave runs as
/// that user and writes the file), and `root` itself when it is brave-act's own download folder (`own`); a
/// symbolic link below `root` is refused, so nothing outside is made or handed over.
fn make_dir(root: &Path, dir: &Path, own: bool) -> std::io::Result<()> {
    let as_root = euid() == Some(0);
    let give = |p: &Path| if as_root { std::os::unix::fs::chown(p, Some(1000), Some(1000)) } else { Ok(()) };
    if !root.exists() {
        std::fs::create_dir_all(root)?;
        give(root)?;
    } else if own && std::fs::symlink_metadata(root).is_ok_and(|m| m.is_dir()) {
        give(root)?;
    }
    let mut p = root.to_path_buf();
    for c in dir.strip_prefix(root).map_err(std::io::Error::other)?.components() {
        p.push(c);
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(std::io::Error::other(format!("{} is no folder (a file or a link)", p.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&p)?;
                give(&p)?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn euid() -> Option<u32> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines().find(|l| l.starts_with("Uid:"))?.split_whitespace().nth(2)?.parse().ok()
}

/// Width and height from a PNG's IHDR.
pub fn png_size(p: &[u8]) -> Option<(u32, u32)> {
    if p.len() < 24 || &p[..8] != b"\x89PNG\r\n\x1a\n" || &p[12..16] != b"IHDR" {
        return None;
    }
    Some((u32::from_be_bytes(p[16..20].try_into().ok()?), u32::from_be_bytes(p[20..24].try_into().ok()?)))
}

fn shot(t: &Tab<'_>, ctx: &Ctx, clip: Option<(f64, f64, f64)>) -> R<Vec<u8>> {
    let mut p = vec![("format", Json::str("png")), ("fromSurface", Json::Bool(true))];
    if let Some((w, h, s)) = clip {
        p.push(("clip", Json::obj(vec![("x", Json::Num(0.0)), ("y", Json::Num(0.0)), ("width", Json::Num(w)), ("height", Json::Num(h)), ("scale", Json::Num(s))])));
    }
    let r = t.call("Page.captureScreenshot", Json::obj(p), ctx)?;
    let data = r.get("data").and_then(Json::as_str).ok_or_else(|| Reply::error("Page.captureScreenshot gave no data"))?;
    bitsact::base64::decode(data).map_err(|e| Reply::error(format!("the screenshot: {e}")))
}

#[cfg(test)]
mod tests;
