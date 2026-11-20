//! EINS-B tests: a small scripted fake of `assist serve`'s protocol v2 (PLAN K1) on the App's own connection
//! (`Client::detached`): it answers `status`, `chats`, `new` and `history` as the contract says and keeps every
//! other request line for the test to look at; events are pushed in as the service would send them. Wave A's
//! `assist-fake-serve` replaces it once it is there (eins-b.md).

use std::sync::mpsc::Receiver;

use mcpreg::Json;
use smithay::reexports::calloop::channel;
use smithay::reexports::wayland_server::Display;

use crate::assist_client::{Client, Msg};
use crate::state::App;

pub struct Fake {
    rx: Receiver<String>,
    chats: Vec<(String, String)>,
    /// Request lines the fake does not answer itself (ask, answer, cancel, desk_result, remove ...).
    pub seen: Vec<Json>,
    pub place: &'static str,
}

/// An App with the popup's channel, connected to a ready fake assistant (in the cloud).
pub fn app() -> (Display<App>, App, Fake) {
    let d = Display::<App>::new().expect("display");
    let mut a = App::new(d.handle(), "eins-test".into(), (2000, 1125));
    let (tx, _rx) = channel::channel::<(u64, crate::db_worker::Msg)>();
    a.db.tx = Some(tx);
    let (client, rx) = Client::detached();
    a.assist_client = Some(client);
    let mut f = Fake { rx, chats: Vec::new(), seen: Vec::new(), place: "cloud" };
    a.assist_msg(Msg::Up);
    f.pump(&mut a);
    (d, a, f)
}

impl Fake {
    fn reply(&mut self, a: &mut App, s: &str) {
        a.assist_msg(Msg::Line(Json::parse(s).expect("a JSON line")));
    }

    /// Answers what the App asked for until nothing new comes.
    pub fn pump(&mut self, a: &mut App) {
        for _ in 0..20 {
            let lines: Vec<String> = self.rx.try_iter().collect();
            if lines.is_empty() {
                return;
            }
            for l in lines {
                let j = Json::parse(&l).expect("the App sends JSON lines");
                match j.get("op").and_then(Json::as_str) {
                    Some("status") => {
                        let s = format!(r#"{{"ok":true,"name":"Claude","provider":"abo","place":"{}","local_only":false,"ready":true,"busy":false,"turns":0}}"#, self.place);
                        self.reply(a, &s);
                    }
                    Some("chats") => {
                        let rows: Vec<String> = self.chats.iter().enumerate().map(|(i, (c, o))| format!(r#"{{"chat":"{c}","title":"Chat {c}","origin":"{o}","updated":{},"turns":1}}"#, 1000 + i)).collect();
                        let s = format!(r#"{{"ok":true,"chats":[{}]}}"#, rows.join(","));
                        self.reply(a, &s);
                    }
                    Some("new") => {
                        let c = format!("c{}", self.chats.len() + 1);
                        self.chats.push((c.clone(), j.get("origin").and_then(Json::as_str).unwrap_or("sidebar").to_string()));
                        self.reply(a, &format!(r#"{{"ok":true,"chat":"{c}"}}"#));
                    }
                    Some("history") => {
                        let c = j.get("chat").and_then(Json::as_str).unwrap_or("");
                        let s = format!(r#"{{"ok":true,"chat":"{c}","turns":[{{"who":"georg","text":"such nach Georg"}},{{"who":"tool","name":"handeln","line":"In die Adresszeile schreiben","state":"done"}},{{"who":"assistant","text":"Erledigt."}}]}}"#);
                        self.reply(a, &s);
                    }
                    _ => self.seen.push(j),
                }
            }
        }
    }

    /// The request lines of `op` sent so far (taken out).
    pub fn got(&mut self, a: &mut App, op: &str) -> Vec<Json> {
        self.pump(a);
        let (mine, rest): (Vec<Json>, Vec<Json>) = std::mem::take(&mut self.seen).into_iter().partition(|j| j.get("op").and_then(Json::as_str) == Some(op));
        self.seen = rest;
        mine
    }

    /// An event of chat `chat` (`body`: the rest of the object, e.g. `"text":"Hallo"`).
    pub fn ev(&mut self, a: &mut App, chat: &str, ev: &str, body: &str) {
        let s = if body.is_empty() { format!(r#"{{"ev":"{ev}","chat":"{chat}"}}"#) } else { format!(r#"{{"ev":"{ev}","chat":"{chat}",{body}}}"#) };
        self.reply(a, &s);
        self.pump(a);
    }

    /// A desktop tool's call of chat `chat`.
    pub fn desk(&mut self, a: &mut App, chat: &str, call: &str, name: &str, args: serde_json::Value) {
        let s = serde_json::json!({"ev": "desk", "chat": chat, "call_id": call, "name": name, "args": args}).to_string();
        self.reply(a, &s);
        self.pump(a);
    }

    /// The `desk_result` of call `call`: its text and whether it is an error.
    pub fn result(&mut self, a: &mut App, call: &str) -> Option<(String, bool)> {
        let all = self.got(a, "desk_result");
        let mut found = None;
        for j in all {
            if j.get("call_id").and_then(Json::as_str) == Some(call) && found.is_none() {
                found = Some((j.get("content").and_then(Json::as_str).unwrap_or("").to_string(), j.get("is_error").and_then(Json::as_bool).unwrap_or(false)));
            } else {
                self.seen.push(j);
            }
        }
        found
    }
}
