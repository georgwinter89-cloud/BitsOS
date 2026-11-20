//! CX-5: the context service's tools (`context_search`, `workspace_*`, ...). ctxd runs as Georg's uid 1000, and
//! the kernel's local tools socket takes services of uid 0 only, so the assistant asks ctxd itself on its
//! socket (`$XDG_RUNTIME_DIR/ctxd.sock`, op `mcp`: one JSON-RPC message of ctxd's MCP door), as it asks the
//! desktop for `program_start`. That door is ctxd's view for an AI: title, place and a short snippet, never a
//! private folder's documents (ctxd `api.rs`, `Caller::Mcp`).
//!
//! What a tool may do is this program's table, not ctxd's word: searching and looking read and run at once,
//! changing a workspace asks Georg first, forgetting is not offered (Georg does that himself). The list
//! ctxd sends is bounded as the kernel's is (`local::list`), and only names of the table are taken.
//! The listener must be this user's, checked on every connection.

use super::local::list;
use super::{Class, ToolAnswer, ToolSpec};
use crate::json::{self, Json};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The tools taken from ctxd and what each may do.
pub const TABLE: &[(&str, Class)] = &[
    ("context_search", Class::Read),
    ("context_status", Class::Read),
    ("context_explain", Class::Read),
    ("workspace_list", Class::Read),
    ("workspace_open", Class::Change),
    ("workspace_add", Class::Change),
    ("workspace_rename", Class::Change),
    ("workspace_pin", Class::Change),
    ("dismiss", Class::Change),
];
/// The list is an answer from memory; a ctxd that is slower than this is left out of the chat.
const LIST_WAIT: Duration = Duration::from_secs(3);
/// ctxd answers a search in its MCP deadline (3 s); opening a workspace from a search takes a little more.
const CALL_WAIT: Duration = Duration::from_secs(30);
/// The longest answer line read (a search of 500 hits with snippets is well below).
const ANSWER_MAX: u64 = 4 << 20;

pub struct ContextTools {
    path: PathBuf,
    specs: Vec<ToolSpec>,
}

/// The socket in `run`, when one is there.
pub fn socket_in(run: &Path) -> Option<PathBuf> {
    let p = run.join("ctxd.sock");
    std::fs::metadata(&p).is_ok_and(|m| m.file_type().is_socket()).then_some(p)
}

impl ContextTools {
    /// This user's context service, when it runs and answers its list in time.
    pub fn find() -> Option<ContextTools> {
        let uid = usys::geteuid().ok()?;
        let run = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()).map_or_else(|| PathBuf::from(format!("/run/user/{uid}")), PathBuf::from);
        ContextTools::open(&socket_in(&run)?).ok()
    }

    pub fn open(path: &Path) -> Result<ContextTools, String> {
        let mut c = ContextTools { path: path.to_path_buf(), specs: Vec::new() };
        let list_answer = c.rpc(&Json::obj().with("jsonrpc", "2.0").with("id", 1i64).with("method", "tools/list"), LIST_WAIT)?;
        let (specs, _) = list::specs(&list_answer);
        c.specs = specs
            .into_iter()
            .filter_map(|mut s| {
                s.class = TABLE.iter().find(|(n, _)| *n == s.name)?.1;
                Some(s)
            })
            .collect();
        Ok(c)
    }

    pub fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    /// Only the tools `keep` says yes to stay (a name the chat has from elsewhere goes there).
    pub fn keep(&mut self, keep: impl Fn(&str) -> bool) {
        self.specs.retain(|s| keep(&s.name));
    }

    pub fn has(&self, name: &str) -> bool {
        self.specs.iter().any(|s| s.name == name)
    }

    /// One JSON-RPC message through ctxd's socket; the `result` of its answer.
    fn rpc(&self, msg: &Json, wait: Duration) -> Result<Json, String> {
        let not_there = |e: std::io::Error| format!("the context service does not answer ({e})");
        let mut s = UnixStream::connect(&self.path).map_err(not_there)?;
        let uid = usys::geteuid().ok().and_then(|u| u32::try_from(u).ok());
        let peer = settingsd::peer::of(&s);
        if peer.uid.is_none() || peer.uid != uid {
            return Err(format!("{} is not this user's context service (pid {} listens)", self.path.display(), peer.pid));
        }
        s.set_write_timeout(Some(wait)).and_then(|()| s.set_read_timeout(Some(wait))).map_err(not_there)?;
        let line = Json::obj().with("id", 1i64).with("op", "mcp").with("args", msg.clone()).to_text();
        s.write_all(format!("{line}\n").as_bytes()).map_err(not_there)?;
        let mut answer = String::new();
        BufReader::new((&s).take(ANSWER_MAX)).read_line(&mut answer).map_err(not_there)?;
        let a = json::parse(answer.trim()).map_err(|e| format!("the context service's answer is not JSON: {e}"))?;
        if a.get("ok").and_then(Json::as_bool) != Some(true) {
            return Err(format!("the context service refused: {}", a.get("error").map(Json::to_text).unwrap_or_default()));
        }
        let rpc = a.get("result").ok_or("the context service sent no result")?;
        if let Some(e) = rpc.get("error") {
            return Err(format!("the context service: {}", e.str("message").unwrap_or("error")));
        }
        rpc.get("result").cloned().ok_or_else(|| "the context service sent no result".into())
    }

    pub fn call(&self, name: &str, args: &Json) -> ToolAnswer {
        if !self.has(name) {
            return Err(format!("unknown tool {name:?}"));
        }
        let msg = Json::obj()
            .with("jsonrpc", "2.0")
            .with("id", 2i64)
            .with("method", "tools/call")
            .with("params", Json::obj().with("name", name).with("arguments", args.clone()));
        let r = self.rpc(&msg, CALL_WAIT)?;
        let mut text = String::new();
        if let Some(Json::Arr(parts)) = r.get("content") {
            for p in parts.iter().filter_map(|p| p.str("text")) {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(p);
            }
        }
        if r.get("isError").and_then(Json::as_bool) == Some(true) { Err(text) } else { Ok(text) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn tool(name: &str) -> String {
        format!(r#"{{"name":"{name}","description":"d","inputSchema":{{"type":"object","properties":{{}}}},"annotations":{{"readOnlyHint":true}}}}"#)
    }

    #[test]
    fn the_table_decides_the_class_and_forgetting_is_not_offered() {
        let d = std::env::temp_dir().join(format!("assist-ctx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("ctxd.sock");
        let l = UnixListener::bind(&path).unwrap();
        let fake = std::thread::spawn(move || {
            let mut got = Vec::new();
            let list = format!("[{},{},{},{}]", tool("context_search"), tool("workspace_rename"), tool("context_forget"), tool("evil_tool"));
            for answer in [
                format!(r#"{{"id":1,"ok":true,"result":{{"jsonrpc":"2.0","id":1,"result":{{"tools":{list}}}}}}}"#),
                r#"{"id":1,"ok":true,"result":{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"3 Treffer"}],"isError":false}}}"#.to_string(),
                r#"{"id":1,"ok":true,"result":{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"not_found: ws"}],"isError":true}}}"#.to_string(),
            ] {
                let (mut s, _) = l.accept().unwrap();
                let mut line = String::new();
                BufReader::new(&mut s).read_line(&mut line).unwrap();
                got.push(line);
                s.write_all(format!("{answer}\n").as_bytes()).unwrap();
            }
            got
        });
        let c = ContextTools::open(&path).unwrap();
        let names: Vec<(&str, Class)> = c.specs().iter().map(|s| (s.name.as_str(), s.class)).collect();
        assert_eq!(names, [("context_search", Class::Read), ("workspace_rename", Class::Change)], "ctxd's readOnlyHint does not make a rename a read");
        assert_eq!(c.call("context_search", &Json::obj().with("query", "Steuer")), Ok("3 Treffer".into()));
        assert_eq!(c.call("workspace_rename", &Json::obj()), Err("not_found: ws".into()));
        assert!(c.call("context_forget", &Json::obj()).is_err(), "never sent");
        let got = fake.join().unwrap();
        assert!(got[0].contains(r#""op":"mcp""#) && got[0].contains("tools/list"), "{}", got[0]);
        assert!(got[1].contains(r#""name":"context_search""#) && got[1].contains("Steuer"), "{}", got[1]);
        assert_eq!(got.len(), 3);
        assert_eq!(socket_in(&d), Some(path));
        let _ = std::fs::remove_dir_all(&d);
    }
}
