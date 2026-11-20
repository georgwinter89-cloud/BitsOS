//! The machine's own tools (CL-T): the kernel's local tools socket `/run/bitsos/tools.sock` (MC-L), one
//! JSON-RPC line each way, a new connection per request (the kernel may be reloaded between two questions).
//! The door admits this program by what it is (the boot's /bin/assist), no token. Everything that comes back
//! is data from outside this program: the list is filtered before the model or the permission table sees a
//! tool of it ([`list`]), an answer is bounded by the memory budget and never read as anything but text.

pub mod list;
pub mod wire;

use super::{ToolAnswer, ToolSpec};
use crate::json::{self, Json};
use crate::net::Ctl;
use crate::term::wrap;
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wire::{Fail, To};

pub const SOCKET: &str = "/run/bitsos/tools.sock";
/// The door's own code for "this program is not admitted" (the first and only line, then end of file).
const REFUSED: i64 = -32001;
/// The longest request line the door reads (`mcp::local::LINE_MAX`); a longer one is not sent at all.
const LINE_MAX: usize = 256 * 1024;
/// The door ends every call after 60 s at most and answers; one that is silent a little past that is stuck.
const CALL_WAIT: Duration = Duration::from_secs(65);
/// The list is an answer from memory. Longer than this and the chat starts without the machine's tools.
/// The list is asked before the chat's screen is up: until then Esc means nothing yet and Ctrl-C ends the
/// program as it ends any other, so a silent door costs the start of a chat these seconds at most.
const LIST_WAIT: Duration = Duration::from_secs(5);
/// The most characters of the door's own words in a line for the log.
const SHOWN: usize = 200;

/// Why the machine's tools are not there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// No socket: a kernel without it, or MCP is off (no token in CONFIG.TXT).
    Absent,
    /// The door's reason, in its words.
    Refused(String),
    Hang,
    Broken(String),
    /// Not asked at all (`place`).
    NotAsked(&'static str),
}

impl Why {
    /// One technical line for the log; the door's words are bounded and on one line.
    pub fn log_line(&self) -> String {
        match self {
            Why::Absent => format!("tools: no socket at {SOCKET}"),
            Why::Refused(why) => format!("tools: refused: {}", wrap::inline(why, SHOWN)),
            Why::Hang => "tools: no answer to the list in time".into(),
            Why::Broken(what) => format!("tools: {}", wrap::inline(what, SHOWN)),
            Why::NotAsked(why) => format!("tools: not asked: {why}"),
        }
    }
}

fn why(f: Fail) -> Why {
    match f {
        Fail::Absent(_) => Why::Absent,
        Fail::Hang => Why::Hang,
        Fail::Cancelled => Why::Broken("cancelled".into()),
        Fail::TooBig => Why::Broken("the list is larger than this machine can hold".into()),
        Fail::Closed => Why::Broken("closed without an answer".into()),
        Fail::Door => Why::NotAsked("the tools socket named from outside (TOOLS_SOCKET) is the machine's own"),
        Fail::Io(e) => Why::Broken(e),
    }
}

/// The `result` of the answer to request `id`; Err is the door's refusal or what is wrong with the line.
fn result_of(line: &str, id: u64) -> Result<Json, Why> {
    let answer = json::parse(line.trim()).map_err(|e| Why::Broken(format!("the answer is not JSON: {e}")))?;
    if let Some(e) = answer.get("error") {
        let message = e.str("message").unwrap_or("").to_string();
        return Err(match e.get("code").and_then(Json::as_i64) {
            Some(REFUSED) => Why::Refused(message.strip_prefix("Refused: ").unwrap_or(&message).to_string()),
            code => Why::Broken(format!("error {}: {message}", code.unwrap_or(0))),
        });
    }
    if answer.get("id").and_then(Json::as_i64) != i64::try_from(id).ok() {
        return Err(Why::Broken("the answer belongs to another request".into()));
    }
    answer.get("result").cloned().ok_or_else(|| Why::Broken("an answer without a result".into()))
}

pub struct LocalTools {
    path: PathBuf,
    /// Set for a socket named from outside: the socket it must never be (`wire::To`).
    never: Option<PathBuf>,
    budget: usize,
    specs: Vec<ToolSpec>,
    /// Entries of the list that were left out (`list::specs`).
    pub left_out: usize,
    call_wait: Duration,
    next_id: Cell<u64>,
}

impl LocalTools {
    /// Asks the door for its list, once: the tools of a chat are those of its start.
    pub fn open(path: &Path, budget: usize) -> Result<LocalTools, Why> {
        LocalTools::open_within(path, budget, LIST_WAIT, CALL_WAIT)
    }

    /// A socket named from outside (a test's own world). It is whatever its maker wants except one thing:
    /// `never`, the kernel's door, which admits this program by what it is and would then serve whoever
    /// named the socket. Every connection is checked, not only the first.
    pub fn open_apart(path: &Path, never: &Path, budget: usize) -> Result<LocalTools, Why> {
        LocalTools::start(path, Some(never), budget, LIST_WAIT, CALL_WAIT)
    }

    pub fn open_within(path: &Path, budget: usize, list_wait: Duration, call_wait: Duration) -> Result<LocalTools, Why> {
        LocalTools::start(path, None, budget, list_wait, call_wait)
    }

    fn start(path: &Path, never: Option<&Path>, budget: usize, list_wait: Duration, call_wait: Duration) -> Result<LocalTools, Why> {
        let mut t = LocalTools { path: path.to_path_buf(), never: never.map(Path::to_path_buf), budget, specs: Vec::new(), left_out: 0, call_wait, next_id: Cell::new(1) };
        let (id, line) = t.request("tools/list", None);
        let ctl = Ctl { deadline: Instant::now() + list_wait, budget, cancel: &|| false };
        let answer = wire::exchange(t.to(), &line, &ctl).map_err(why)?;
        (t.specs, t.left_out) = list::specs(&result_of(&answer, id)?);
        Ok(t)
    }

    fn to(&self) -> To<'_> {
        To { path: &self.path, never: self.never.as_deref() }
    }

    pub fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    fn request(&self, method: &str, params: Option<Json>) -> (u64, String) {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let mut req = Json::obj().with("jsonrpc", "2.0").with("id", id).with("method", method);
        if let Some(p) = params {
            req = req.with("params", p);
        }
        (id, req.to_text())
    }

    /// One call. `stop` is asked while the answer is awaited (Esc); the errors are words for the model.
    pub fn call(&self, name: &str, args: &Json, stop: &dyn Fn() -> bool) -> ToolAnswer {
        let (id, line) = self.request("tools/call", Some(Json::obj().with("name", name).with("arguments", args.clone())));
        if line.len() >= LINE_MAX {
            return Err(format!("the arguments are larger than BitsOS takes for one local tool call ({} KiB); send less", LINE_MAX / 1024));
        }
        let ctl = Ctl { deadline: Instant::now() + self.call_wait, budget: self.budget, cancel: stop };
        let answer = wire::exchange(self.to(), &line, &ctl).map_err(|f| match f {
            Fail::Cancelled => "stopped: Georg pressed Esc while the tool ran".to_string(),
            Fail::Hang => "the tool did not answer in time; the call was given up".to_string(),
            Fail::TooBig => "the tool's answer is larger than this machine can hold; ask for less".to_string(),
            Fail::Absent(_) => "BitsOS's local tools are not there any more (the socket is gone)".to_string(),
            Fail::Closed => "BitsOS closed the connection without an answer".to_string(),
            Fail::Door => "this chat's tools socket was named from outside and now leads to BitsOS's own; it is not used".to_string(),
            Fail::Io(e) => format!("the local tools socket failed: {e}"),
        })?;
        let result = result_of(&answer, id).map_err(|w| match w {
            Why::Refused(why) => format!("BitsOS refused this program at the local tools socket: {why}"),
            Why::Broken(what) => what,
            other => other.log_line(),
        })?;
        let mut text = String::new();
        if let Some(Json::Arr(parts)) = result.get("content") {
            for p in parts {
                if !text.is_empty() {
                    text.push('\n');
                }
                match (p.str("type"), p.str("text")) {
                    (Some("text"), Some(t)) => text.push_str(t),
                    (kind, _) => text.push_str(&format!("[{} left out: the chat takes text only]", wrap::inline(kind.unwrap_or("part"), 40))),
                }
            }
        }
        if result.get("isError").and_then(Json::as_bool) == Some(true) { Err(text) } else { Ok(text) }
    }
}

/// Where the machine's tools are asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// The kernel's door.
    Door,
    /// A socket named from outside (`TOOLS_SOCKET`): a test's own world.
    Named(PathBuf),
    Nowhere(&'static str),
}

/// The door proves which program runs, not who drives it (plan/spec/net.md §11.6): a program of
/// Georg's user could start the real assistant with `SETTINGS_SOCKET` at a settings service of its own (which
/// then names the attacker's server as the provider) or with `SSL_CERT_FILE` at its own certificate, and the
/// "model" would be the attacker, calling the machine's tools through the admitted program. So an assistant
/// whose settings or certificates were named from outside does not go to the door at all. Nobody is exempt:
/// "root" is no proof here, a program is root inside a user namespace it made itself.
/// A tools socket named from outside is taken only together with a settings socket named from outside (a
/// whole test world), and is never the door itself (`LocalTools::open_apart`).
/// This stops the puppet whose strings are environment variables and arguments. One whose strings are a
/// mount or user namespace of its own (other files under the fixed paths) only the kernel's door and
/// settingsd can turn away: plan/spec/assistant.md §2, "What the door's identity does not stop".
pub fn place(settings_named: bool, certs_named: bool, tools_named: Option<PathBuf>) -> Place {
    match (settings_named, tools_named) {
        (true, Some(p)) => Place::Named(p),
        (true, None) => Place::Nowhere("the settings socket was named from outside"),
        (false, _) if certs_named => Place::Nowhere("the certificates were named from outside (SSL_CERT_FILE)"),
        (false, _) => Place::Door,
    }
}
