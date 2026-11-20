//! `assist --mcp` with a scripted client on a pipe, against a stand-in for the kernel's tools socket and a
//! settingsd in this process: what is listed, what runs, what never runs, and a client that misbehaves.

mod common;
use assist::json::{self, Json};
use assist::mcp::gate::{Gate, Owner};
use assist::provider::program::ask::{Reply, Why as No};
use assist::mcp::{self, End, LINE_MAX, MCP_SOURCE, Named, Opts, VERSIONS};
use assist::tools::both::{ONLY_SETTINGS, Tools};
use assist::tools::desktop::Desktop;
use assist::tools::local::{LocalTools, Why};
use common::door::{Door, NO_ARGS, Say, listing, text, tool};
use common::{KEY, World};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const BUDGET: usize = 64 << 20;
const VERSION: &str = "BitsOS V2 kernel 0.2.0 (test door)";

/// The kernel's list as the chat tests use it, plus one tool that changes something, one that deletes, and
/// two that take their time.
fn door(name: &str) -> Door {
    Door::start(name, |req, _| {
        let called = req.get("params").and_then(|p| p.str("name")).unwrap_or("").to_string();
        match (req.str("method"), called.as_str()) {
            (Some("tools/list"), _) => {
                let mut all: Vec<Json> = ["cpus", "sched", "version", "slow", "stuck"].iter().map(|n| tool(n, "read", "kernel", NO_ARGS)).collect();
                all.push(tool("metrics", "read", "kernel", r#"{"type":"object","properties":{"name":{"type":"string"}},"additionalProperties":false}"#));
                all.push(tool("restart_sound", "change", "kernel", NO_ARGS));
                all.push(tool("wipe_cache", "delete_overwrite", "kernel", NO_ARGS));
                for (n, safety) in [("settings_list", "read"), ("settings_get", "read"), ("settings_set", "change")] {
                    all.push(tool(n, safety, "settingsd", r#"{"type":"object"}"#));
                }
                listing(all)
            }
            (Some("tools/call"), "version") => text(VERSION, false),
            (Some("tools/call"), "slow") => {
                std::thread::sleep(Duration::from_millis(1500));
                text("slow done", false)
            }
            (Some("tools/call"), "stuck") => Say::Hang,
            (Some("tools/call"), n) => text(&format!("{n} done"), false),
            _ => Say::Error(-32601, "no".into()),
        }
    })
}

struct Client {
    to: Option<UnixStream>,
    from: BufReader<UnixStream>,
    next: i64,
    /// Everything the server ever wrote to this client.
    seen: String,
}

/// A tool call's outcome: the text and `isError`, or the JSON-RPC error code.
type Called = Result<(String, bool), i64>;

impl Client {
    fn send(&mut self, line: &str) {
        let to = self.to.as_mut().unwrap();
        to.write_all(line.as_bytes()).unwrap();
        to.write_all(b"\n").unwrap();
    }
    /// The next line from the server; None when it wrote nothing within `wait`.
    fn read_within(&mut self, wait: Duration) -> Option<Json> {
        self.from.get_ref().set_read_timeout(Some(wait)).unwrap();
        let mut line = String::new();
        match self.from.read_line(&mut line) {
            Ok(n) if n > 0 => {
                self.seen.push_str(&line);
                Some(json::parse(line.trim()).unwrap_or_else(|e| panic!("the server wrote something that is not JSON ({e}): {line}")))
            }
            _ => None,
        }
    }
    fn read(&mut self) -> Json {
        self.read_within(Duration::from_secs(20)).expect("no answer from the server")
    }
    fn ask(&mut self, method: &str, params: Option<Json>) -> i64 {
        self.next += 1;
        let mut req = Json::obj().with("jsonrpc", "2.0").with("id", self.next).with("method", method);
        if let Some(p) = params {
            req = req.with("params", p);
        }
        self.send(&req.to_text());
        self.next
    }
    fn request(&mut self, method: &str, params: Option<Json>) -> Json {
        let id = self.ask(method, params);
        let answer = self.read();
        assert_eq!(answer.get("id").and_then(Json::as_i64), Some(id), "{}", answer.to_text());
        assert_eq!(answer.str("jsonrpc"), Some("2.0"));
        answer
    }
    fn start_call(&mut self, name: &str, args: &str) -> i64 {
        self.ask("tools/call", Some(Json::obj().with("name", name).with("arguments", json::parse(args).unwrap())))
    }
    fn outcome(answer: &Json) -> Called {
        if let Some(e) = answer.get("error") {
            return Err(e.get("code").and_then(Json::as_i64).unwrap());
        }
        let r = answer.get("result").unwrap();
        let Some(Json::Arr(parts)) = r.get("content") else { panic!("no content: {}", answer.to_text()) };
        Ok((parts.iter().filter_map(|p| p.str("text")).collect(), r.get("isError").and_then(Json::as_bool).unwrap()))
    }
    fn call(&mut self, name: &str, args: &str) -> Called {
        let id = self.start_call(name, args);
        let answer = self.read();
        assert_eq!(answer.get("id").and_then(Json::as_i64), Some(id), "{}", answer.to_text());
        Client::outcome(&answer)
    }
    /// The listed tools: name and the two hints.
    fn tools(&mut self) -> Vec<(String, bool, bool)> {
        let a = self.request("tools/list", None);
        let Some(Json::Arr(tools)) = a.get("result").and_then(|r| r.get("tools")) else { panic!("{}", a.to_text()) };
        let hint = |t: &Json, k: &str| t.get("annotations").and_then(|a| a.get(k)).and_then(Json::as_bool).unwrap();
        tools
            .iter()
            .map(|t| {
                assert!(t.get("inputSchema").is_some_and(|s| s.str("type") == Some("object")) && t.str("description").is_some());
                (t.str("name").unwrap().to_string(), hint(t, "readOnlyHint"), hint(t, "destructiveHint"))
            })
            .collect()
    }
    fn names(&mut self) -> Vec<String> {
        self.tools().into_iter().map(|t| t.0).collect()
    }
    fn error_code(&mut self) -> (Json, i64) {
        let a = self.read();
        (a.get("id").cloned().unwrap(), a.get("error").and_then(|e| e.get("code")).and_then(Json::as_i64).unwrap_or_else(|| panic!("{}", a.to_text())))
    }
}

/// Runs the server on a thread with pipes to `script`, and returns the script's result and why the server
/// ended once the script has closed its side.
fn serve<T>(w: &World, door: Option<&Door>, opts: Opts, script: impl FnOnce(&mut Client) -> T) -> (T, End) {
    serve_under(w, door, opts, None, script)
}

/// The same with an assistant above the server that can ask Georg.
fn serve_under<T>(w: &World, door: Option<&Door>, opts: Opts, owner: Option<&(dyn Owner + Sync)>, script: impl FnOnce(&mut Client) -> T) -> (T, End) {
    serve_on_desktop(w, door, None, opts, owner, script)
}

/// The same with the desktop's control socket at `desktop` (its `program_start`).
fn serve_on_desktop<T>(w: &World, door: Option<&Door>, desktop: Option<&Path>, opts: Opts, owner: Option<&(dyn Owner + Sync)>, script: impl FnOnce(&mut Client) -> T) -> (T, End) {
    let (client_to, server_in) = UnixStream::pair().unwrap();
    let (server_out, client_from) = UnixStream::pair().unwrap();
    std::thread::scope(|s| {
        let server = s.spawn(move || {
            let named = Named(w);
            let local = door.map_or(Err(Why::Absent), |d| LocalTools::open_within(&d.path, BUDGET, Duration::from_secs(5), Duration::from_secs(20)));
            let tools = Tools::new(&named, local).with_desktop(desktop.map(|p| Desktop::at(p.to_path_buf())));
            mcp::serve(server_in, server_out, &Gate { settings: &named, tools: &tools, read_only: opts.read_only, owner: owner.map(|o| o as &dyn Owner) }, &opts).unwrap()
        });
        let mut c = Client { to: Some(client_to), from: BufReader::new(client_from), next: 0, seen: String::new() };
        let out = script(&mut c);
        c.to = None;
        (out, server.join().unwrap())
    })
}

fn open() -> Opts {
    Opts::new(false)
}

#[test]
fn the_list_and_a_call_that_reads() {
    let w = World::new("mcp-list");
    let d = door("list");
    let (_, end) = serve(&w, Some(&d), open(), |c| {
        // The client's revision when the server speaks it, else the server's newest.
        for (asked, got) in [("2025-06-18", "2025-06-18"), ("2024-11-05", "2024-11-05"), ("2099-01-01", VERSIONS[0]), ("", VERSIONS[0])] {
            let hello = c.request("initialize", Some(Json::obj().with("protocolVersion", asked).with("capabilities", Json::obj()).with("clientInfo", Json::obj().with("name", "t").with("version", "1"))));
            let r = hello.get("result").unwrap();
            assert_eq!(r.str("protocolVersion"), Some(got));
            assert!(r.get("capabilities").and_then(|c| c.get("tools")).is_some() && r.get("serverInfo").and_then(|s| s.str("name")) == Some("bitsos-assist"));
        }
        // A notification is never answered: the next answer is the ping's.
        c.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        assert_eq!(c.request("ping", None).get("result"), Some(&Json::obj()));

        let tools = c.tools();
        let names: Vec<&str> = tools.iter().map(|t| t.0.as_str()).collect();
        assert_eq!(
            names,
            ["settings_list", "settings_describe", "settings_get", "settings_set", "settings_undo", "settings_history", "cpus", "sched", "version", "slow", "stuck", "metrics", "restart_sound", "wipe_cache"],
            "settingsd's six once, then the machine's"
        );
        let hints = |n: &str| tools.iter().find(|t| t.0 == n).map(|t| (t.1, t.2)).unwrap();
        assert_eq!(hints("version"), (true, false));
        assert_eq!(hints("settings_get"), (true, false));
        assert_eq!(hints("settings_set"), (false, false), "a change that can be undone");
        assert_eq!(hints("restart_sound"), (false, false));
        assert_eq!(hints("wipe_cache"), (false, true));

        assert_eq!(c.call("version", "{}"), Ok((VERSION.to_string(), false)));
        let (scale, is_error) = c.call("settings_get", r#"{"key":"theme.scale"}"#).unwrap();
        assert!(!is_error && scale.contains("125"), "{scale}");
        // No arguments at all is an empty object.
        let id = c.ask("tools/call", Some(Json::obj().with("name", "cpus")));
        assert_eq!((c.read().get("id").and_then(Json::as_i64), d.called()), (Some(id), vec!["version".to_string(), "cpus".to_string()]));
    });
    assert_eq!(end, End::Closed);
    assert_eq!(d.lists(), 1, "the door is asked for its list once");
}

#[test]
fn what_the_chat_would_ask_runs_only_when_georg_allowed_it() {
    let w = World::new("mcp-ask");
    w.provider("p", "anthropic", "https://api.example.invalid", "cloud", Some(KEY));
    let d = door("ask");
    let (seen, _) = serve(&w, Some(&d), open(), |c| {
        let how = "settings --confirm set ai.mcp.ask_tools true als root an der Konsole";
        // On by default (DECISIONS 18.11.); Georg turned it off: listed, and every such call says in one sentence
        // how to allow it.
        assert_eq!(w.value("ai.mcp.ask_tools").as_bool(), Some(true));
        w.set("ai.mcp.ask_tools", false);
        w.set("theme.scale", 130i64);
        for (name, args) in [("settings_set", r#"{"key":"theme.scale","value":150}"#), ("settings_set", r#"{"key":"theme.scale","value":150,"confirmed":true}"#), ("settings_undo", "{}"), ("restart_sound", "{}"), ("wipe_cache", "{}")] {
            let (said, is_error) = c.call(name, args).unwrap();
            assert!(is_error && said.contains(how) && said.contains(name) && !said.contains('\n'), "{name}: {said}");
        }
        assert_eq!((w.value("theme.scale").as_i64(), d.called().len()), (Some(130), 0));

        w.set("ai.mcp.ask_tools", true);
        let (said, is_error) = c.call("settings_set", r#"{"key":"theme.scale","value":150}"#).unwrap();
        assert!(!is_error && w.value("theme.scale").as_i64() == Some(150), "{said}");
        let history = w.request_text(r#"{"op":"history","n":1}"#);
        assert!(history.contains(&format!(r#""source":"{MCP_SOURCE}""#)), "journaled as this mode's change: {history}");
        assert_eq!(c.call("restart_sound", "{}"), Ok(("restart_sound done".to_string(), false)));
        assert_eq!(c.call("settings_undo", "{}").map(|r| r.1), Ok(false));
        assert_eq!(w.value("theme.scale").as_i64(), Some(130));

        // The assistant's own leash: never, whatever the level. Also not by taking a change of it back.
        w.set("ai.level", "auto");
        for (key, value) in [("ai.local_only", "true"), ("ai.mcp.ask_tools", "false"), ("ai.level", r#""answer""#), ("ai.role.chat", r#""p""#), ("ai.provider.p.key", r#""x""#), ("ai.provider.p.address", r#""https://elsewhere.invalid""#)] {
            let before = w.request_text(&format!(r#"{{"op":"get","key":"{key}"}}"#));
            let (said, is_error) = c.call("settings_set", &format!(r#"{{"key":"{key}","value":{value},"confirmed":true}}"#)).unwrap();
            assert!(is_error && said.contains("„ai.“") && said.contains("nie ändern"), "{key}: {said}");
            assert_eq!(w.request_text(&format!(r#"{{"op":"get","key":"{key}"}}"#)), before, "{key}");
        }
        w.set("ai.local_only", true);
        let (said, is_error) = c.call("settings_undo", "{}").unwrap();
        assert!(is_error && said.contains("nicht zurücknehmen") && w.value("ai.local_only").as_bool() == Some(true), "{said}");

        // What the chat refuses outright stays refused.
        for key in ["nvme_write", "kernel.nvme_write", "wlan_psk", "token"] {
            let (said, is_error) = c.call("settings_set", &format!(r#"{{"key":"{key}","value":"on"}}"#)).unwrap();
            assert!(is_error && said.starts_with("refused_r8"), "{key}: {said}");
        }

        // A secret is never read: not by name, not in a list, not in the history.
        for (name, args) in [("settings_get", r#"{"key":"ai.provider.p.key"}"#), ("settings_describe", r#"{"key":"ai.provider.p.key"}"#), ("settings_list", r#"{"prefix":"ai"}"#), ("settings_history", "{}")] {
            assert!(c.call(name, args).is_ok(), "{name}");
        }

        // Nur antworten takes every changing tool out: not listed, and a call finds no such tool.
        w.set("ai.level", "answer");
        let names = c.names();
        assert!(!names.iter().any(|n| ["settings_set", "settings_undo", "restart_sound", "wipe_cache"].contains(&n.as_str())) && names.contains(&"cpus".to_string()) && names.contains(&"settings_history".to_string()), "{names:?}");
        let before = d.called().len();
        assert_eq!(c.call("restart_sound", "{}"), Err(-32602));
        assert_eq!(c.call("settings_set", r#"{"key":"theme.scale","value":150}"#), Err(-32602));
        assert_eq!(c.call("no_such_tool", "{}"), Err(-32602));
        assert_eq!(d.called().len(), before);
        c.seen.clone()
    });
    assert!(seen.contains("***") && !seen.contains(KEY), "the key showed up in what the client read");
    assert!(!w.everything_settingsd_kept().contains(KEY));
}

#[test]
fn read_only_offers_what_reads_whatever_is_set() {
    let w = World::new("mcp-read-only");
    w.set("ai.mcp.ask_tools", true);
    w.set("ai.level", "auto");
    let d = door("read-only");
    serve(&w, Some(&d), Opts::new(true), |c| {
        let tools = c.tools();
        assert_eq!(tools.iter().map(|t| t.0.as_str()).collect::<Vec<_>>(), ["settings_list", "settings_describe", "settings_get", "settings_history", "cpus", "sched", "version", "slow", "stuck", "metrics"]);
        assert!(tools.iter().all(|t| t.1 && !t.2), "every tool here only reads");
        for name in ["settings_set", "settings_undo", "restart_sound", "wipe_cache"] {
            let args = if name == "settings_set" { r#"{"key":"theme.scale","value":150}"# } else { "{}" };
            assert_eq!(c.call(name, args), Err(-32602), "{name}");
        }
        assert_eq!(c.call("version", "{}"), Ok((VERSION.to_string(), false)));
        assert_eq!((w.value("theme.scale").as_i64(), d.called()), (Some(125), vec!["version".to_string()]));
    });
}

#[test]
fn a_client_that_sends_garbage_gets_errors_and_the_server_goes_on() {
    let w = World::new("mcp-garbage");
    let d = door("garbage");
    let (_, end) = serve(&w, Some(&d), open(), |c| {
        let null = Json::Null;
        let expect = |c: &mut Client, line: &str, id: &Json, code: i64| {
            c.send(line);
            assert_eq!(c.error_code(), (id.clone(), code), "{}", &line[..line.len().min(80)]);
        };
        expect(c, "this is no JSON", &null, -32700);
        expect(c, "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":", &null, -32700);
        expect(c, &"[".repeat(100_000), &null, -32700);
        expect(c, "5", &null, -32600);
        expect(c, "\"ping\"", &null, -32600);
        expect(c, "[]", &null, -32600);
        expect(c, r#"{"jsonrpc":"2.0","id":7}"#, &Json::from(7i64), -32600);
        expect(c, r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#, &null, -32600);
        expect(c, r#"{"jsonrpc":"2.0","id":{"a":1},"method":"ping"}"#, &null, -32600);
        expect(c, r#"{"jsonrpc":"2.0","id":"abc","method":"resources/list"}"#, &Json::from("abc"), -32601);
        expect(c, r#"{"jsonrpc":"2.0","id":8,"method":"tools/call"}"#, &Json::from(8i64), -32602);
        expect(c, r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"version","arguments":[1]}}"#, &Json::from(9i64), -32602);
        expect(c, r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":7}}"#, &Json::from(10i64), -32602);
        // A line beyond the bound is not kept; one exactly below it is read.
        expect(c, &"x".repeat(LINE_MAX + 1), &null, -32700);
        expect(c, &"x".repeat(8 * LINE_MAX), &null, -32700);
        expect(c, &format!(r#"{{"jsonrpc":"2.0","id":11,"method":"nothing","params":"{}"}}"#, "y".repeat(LINE_MAX - 100)), &Json::from(11i64), -32601);
        // Not UTF-8.
        c.to.as_mut().unwrap().write_all(b"{\"id\":1,\"method\":\"\xff\xfe\"}\n").unwrap();
        assert_eq!(c.error_code(), (null.clone(), -32700));
        // An unknown method's name comes back bounded and on one line.
        c.send(&format!(r#"{{"jsonrpc":"2.0","id":12,"method":"{}\u001b[2J\nx"}}"#, "m".repeat(5000)));
        let a = c.read();
        let said = a.get("error").and_then(|e| e.str("message")).unwrap();
        assert!(said.len() < 400 && !said.contains('\u{1b}') && !said.contains('\n'), "{said}");

        // Arguments against the tool's schema never reach the tool.
        let (said, is_error) = c.call("metrics", r#"{"name":5,"since":"1970"}"#).unwrap();
        assert!(is_error && said.starts_with("invalid arguments"), "{said}");
        let (said, is_error) = c.call("settings_set", r#"{"key":"theme.scale"}"#).unwrap();
        assert!(is_error && said.contains("value is missing"), "{said}");
        assert!(d.called().is_empty());

        // Things that get no answer at all: blank lines, notifications (known or not), answers to nothing.
        for line in ["", "   ", r#"{"jsonrpc":"2.0","method":"notifications/whatever","params":{"x":1}}"#, r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"version"}}"#, r#"{"jsonrpc":"2.0","id":3,"result":{}}"#, r#"{"jsonrpc":"2.0","id":3,"error":{"code":1,"message":"x"}}"#] {
            c.send(line);
        }
        assert_eq!(c.request("ping", None).get("result"), Some(&Json::obj()));
        assert!(d.called().is_empty(), "a call sent as a notification does not run");

        // A batch is answered as one; notifications in it are left out.
        c.send(r#"[{"jsonrpc":"2.0","id":"a","method":"ping"},{"jsonrpc":"2.0","method":"notifications/x"},7,{"jsonrpc":"2.0","id":"b","method":"nope"}]"#);
        let Json::Arr(answers) = c.read() else { panic!("a batch is answered with a list") };
        assert_eq!(answers.len(), 3);
        assert_eq!((answers[0].str("id"), answers[2].str("id")), (Some("a"), Some("b")));
        c.send(r#"[{"jsonrpc":"2.0","method":"notifications/x"}]"#);

        // Many requests at once, read only afterwards: every one is answered, in order.
        let mut burst = String::new();
        for i in 0..3000 {
            burst.push_str(&format!("{{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"ping\"}}\n", 1000 + i));
        }
        let mut to = c.to.as_ref().unwrap().try_clone().unwrap();
        let writer = std::thread::spawn(move || to.write_all(burst.as_bytes()).unwrap());
        for i in 0..3000 {
            assert_eq!(c.read().get("id").and_then(Json::as_i64), Some(1000 + i));
        }
        writer.join().unwrap();
        // A last line without its line end still counts.
        c.to.as_mut().unwrap().write_all(br#"{"jsonrpc":"2.0","id":"last","method":"ping"}"#).unwrap();
        c.to = None;
        assert_eq!(c.read().str("id"), Some("last"));
    });
    assert_eq!(end, End::Closed);
}

#[test]
fn calls_while_another_runs_wait_their_turn_and_a_cancel_ends_one() {
    let w = World::new("mcp-meanwhile");
    let d = door("meanwhile");
    let (_, end) = serve(&w, Some(&d), open(), |c| {
        // A slow call, then a ping and a second call before it is done: the ping is answered at once, the
        // calls in their order.
        let slow = c.start_call("slow", "{}");
        let ping = c.ask("ping", None);
        let version = c.start_call("version", "{}");
        let order: Vec<i64> = (0..3).map(|_| c.read().get("id").and_then(Json::as_i64).unwrap()).collect();
        assert_eq!(order, [ping, slow, version]);

        // A call that would hang, given up by the client: no answer for it, the door sees the close, and a
        // request that was waiting and is given up too never runs.
        let started = Instant::now();
        let stuck = c.start_call("stuck", "{}");
        let never = c.start_call("restart_sound", "{}");
        // Cancelled only once the call is at the door (earlier, it would simply not be sent).
        for _ in 0..500 {
            if d.called().contains(&"stuck".to_string()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        c.send(&format!(r#"{{"jsonrpc":"2.0","method":"notifications/cancelled","params":{{"requestId":{never},"reason":"x"}}}}"#));
        c.send(&format!(r#"{{"jsonrpc":"2.0","method":"notifications/cancelled","params":{{"requestId":{stuck}}}}}"#));
        let after = c.request("ping", None);
        assert!(after.get("result").is_some() && started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        assert!(!d.called().contains(&"restart_sound".to_string()));
        for _ in 0..100 {
            if d.given_up.load(std::sync::atomic::Ordering::SeqCst) == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(d.given_up.load(std::sync::atomic::Ordering::SeqCst), 1, "the door saw the call given up {:?} {:?}", d.called(), started.elapsed());

        // The client closes its side with calls still on the way (a script through a pipe): they are
        // answered before the server ends.
        let slow = c.start_call("slow", "{}");
        let version = c.start_call("version", "{}");
        c.to = None;
        assert_eq!(c.read().get("id").and_then(Json::as_i64), Some(slow));
        assert_eq!(c.read().get("id").and_then(Json::as_i64), Some(version));
        assert!(c.read_within(Duration::from_secs(5)).is_none(), "then the server closes its side");
    });
    assert_eq!(end, End::Closed);
    assert_eq!(d.called(), ["slow", "version", "stuck", "slow", "version"]);
}

#[test]
fn a_client_that_never_reads_ends_the_server() {
    let w = World::new("mcp-never-reads");
    let d = door("never-reads");
    let started = Instant::now();
    let opts = Opts { read_only: false, write_wait: Duration::from_millis(400) };
    let (writer, end) = serve(&w, Some(&d), opts, |c| {
        // Lists without end, and not one answer is read: the pipe back fills up.
        let mut to = c.to.take().unwrap();
        std::thread::spawn(move || {
            let line = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n";
            while to.write_all(line).is_ok() {}
        })
    });
    assert_eq!(end, End::Stuck);
    assert!(started.elapsed() < Duration::from_secs(30), "{:?}", started.elapsed());
    // With the server gone the client's own writes fail and its thread ends.
    writer.join().unwrap();
}

fn program(w: &World, tools_socket: Option<&std::path::Path>, args: &[&str], input: &str) -> (i32, Vec<Json>, String) {
    let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_assist"));
    c.arg("--socket").arg(&w.socket).args(args).env_remove("TOOLS_SOCKET").env_remove("SSL_CERT_FILE");
    c.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    if let Some(p) = tools_socket {
        c.env("TOOLS_SOCKET", p);
    }
    let mut child = c.spawn().unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let o = child.wait_with_output().unwrap();
    let lines = String::from_utf8(o.stdout).unwrap().lines().map(|l| json::parse(l).unwrap()).collect();
    (o.status.code().unwrap_or(-1), lines, String::from_utf8_lossy(&o.stderr).into_owned())
}

#[test]
fn the_real_program_speaks_mcp() {
    let w = World::new("mcp-program");
    w.listen();
    let d = door("program");
    let script = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"clientInfo\":{\"name\":\"t\",\"version\":\"1\"}}}\n\
{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n\
{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n\
{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"version\",\"arguments\":{}}}\n\
{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"settings_set\",\"arguments\":{\"key\":\"theme.scale\",\"value\":150}}}\n";
    let names = |answer: &Json| -> Vec<String> {
        let Some(Json::Arr(tools)) = answer.get("result").and_then(|r| r.get("tools")) else { panic!("{}", answer.to_text()) };
        tools.iter().map(|t| t.str("name").unwrap().to_string()).collect()
    };
    let said = |answer: &Json| Client::outcome(answer).unwrap();
    // Georg turned the changing tools off (they are on by default).
    w.set("ai.mcp.ask_tools", false);

    let (code, out, stderr) = program(&w, Some(&d.path), &["--mcp"], script);
    assert_eq!((code, out.len()), (0, 4), "nothing but the four answers on stdout: {stderr}");
    assert_eq!(out[0].get("result").and_then(|r| r.str("protocolVersion")), Some("2025-06-18"));
    assert!(names(&out[1]).contains(&"version".to_string()) && names(&out[1]).contains(&"settings_set".to_string()));
    assert_eq!(said(&out[2]), (VERSION.to_string(), false));
    assert!(said(&out[3]).1 && said(&out[3]).0.contains("ai.mcp.ask_tools"));
    assert!(!stderr.contains(ONLY_SETTINGS) && w.value("theme.scale").as_i64() == Some(125), "{stderr}");

    // --read-only, in either order of the two words.
    for args in [["--mcp", "--read-only"], ["--read-only", "--mcp"]] {
        let (code, out, _) = program(&w, Some(&d.path), &args, script);
        assert_eq!(code, 0);
        assert!(names(&out[1]).contains(&"version".to_string()) && !names(&out[1]).contains(&"settings_set".to_string()));
        assert_eq!(out[3].get("error").and_then(|e| e.get("code")).and_then(Json::as_i64), Some(-32602));
    }

    // Without the machine's tools: said on stderr (the client's log of this server), settingsd's six remain.
    let (code, out, stderr) = program(&w, None, &["--mcp"], script);
    assert_eq!(code, 0);
    assert!(stderr.contains(ONLY_SETTINGS) && names(&out[1]).len() == 6, "{stderr}");
    assert_eq!(out[2].get("error").and_then(|e| e.get("code")).and_then(Json::as_i64), Some(-32602));

    // Anything else after --mcp is not a mode.
    let (code, out, stderr) = program(&w, None, &["--mcp", "--write"], "");
    assert!(code == 2 && out.is_empty() && stderr.contains("assist --mcp"), "{stderr}");
}

/// An assistant above the server: writes down what it was asked and answers as told.
struct Above {
    asked: Mutex<Vec<(String, String)>>,
    answer: Mutex<Reply>,
}

impl Owner for Above {
    fn ask(&self, tool: &str, question: &str, _must_ask: bool) -> Reply {
        self.asked.lock().unwrap().push((tool.to_string(), question.to_string()));
        *self.answer.lock().unwrap()
    }
}

#[test]
fn under_an_assistant_a_changing_tool_asks_georg_instead_of_refusing() {
    let w = World::new("mcp-owner");
    let d = door("owner");
    let above = Above { asked: Mutex::new(Vec::new()), answer: Mutex::new(Reply::Yes) };
    let asked = || std::mem::take(&mut *above.asked.lock().unwrap());
    serve_under(&w, Some(&d), open(), Some(&above), |c| {
        let hello = c.request("initialize", None);
        assert!(hello.get("result").and_then(|r| r.str("instructions")).is_some_and(|i| i.contains("BitsOS asks the owner itself")), "{}", hello.to_text());
        // Reading runs, and nobody is asked.
        assert_eq!(c.call("version", "{}"), Ok((VERSION.to_string(), false)));
        assert!(c.call("settings_get", r#"{"key":"theme.scale"}"#).is_ok_and(|r| !r.1) && asked().is_empty());

        // A change: Georg's yes runs it, with the chat's own question.
        let (said, is_error) = c.call("settings_set", r#"{"key":"theme.scale","value":150}"#).unwrap();
        assert!(!is_error && w.value("theme.scale").as_i64() == Some(150), "{said}");
        assert_eq!(asked(), [("settings_set".to_string(), "Einstellung „theme.scale“ ändern: 125 → 150".to_string())]);
        assert_eq!(c.call("restart_sound", "{}"), Ok(("restart_sound done".to_string(), false)));
        assert_eq!(asked().len(), 1);

        // His no, his silence, and nobody to ask: nothing runs, and the model reads why, in German.
        for (why, words) in [(No::Declined, "Georg hat Nein gesagt"), (No::Unanswered, "nicht geantwortet"), (No::CannotAsk, "kann ihn niemand fragen"), (No::Stopped, "abgebrochen")] {
            *above.answer.lock().unwrap() = Reply::No(why);
            let before = d.called().len();
            let (said, is_error) = c.call("settings_set", r#"{"key":"theme.scale","value":175}"#).unwrap();
            assert!(is_error && said.contains(words), "{why:?}: {said}");
            assert!(c.call("wipe_cache", "{}").is_ok_and(|r| r.1));
            assert_eq!((w.value("theme.scale").as_i64(), d.called().len(), asked().len()), (Some(150), before, 2), "{why:?}");
        }

        // A yes does not reach what no yes may reach: the assistant's own leash, and what is no setting.
        *above.answer.lock().unwrap() = Reply::Yes;
        for (key, value) in [("ai.local_only", "true"), ("ai.level", r#""auto""#), ("ai.mcp.ask_tools", "true")] {
            let (said, is_error) = c.call("settings_set", &format!(r#"{{"key":"{key}","value":{value},"confirmed":true}}"#)).unwrap();
            assert!(is_error && said.contains("nie ändern"), "{key}: {said}");
        }
        assert!(c.call("settings_set", r#"{"key":"nvme_write","value":"on"}"#).is_ok_and(|r| r.1 && r.0.starts_with("refused_r8")));
        assert!(asked().is_empty(), "none of these was a question for Georg");
        // `ai.mcp.ask_tools` is for a server nobody stands above: here Georg is asked all the same.
        w.set("ai.mcp.ask_tools", true);
        assert!(c.call("settings_set", r#"{"key":"theme.scale","value":125}"#).is_ok_and(|r| !r.1));
        assert_eq!(asked().len(), 1);
    });

    // Reading only stays reading only, whoever could be asked.
    let (_, end) = serve_under(&w, Some(&d), Opts::new(true), Some(&above), |c| {
        assert_eq!(c.call("settings_set", r#"{"key":"theme.scale","value":150}"#), Err(-32602));
        assert!(!c.names().contains(&"settings_set".to_string()) && asked().is_empty());
    });
    assert_eq!(end, End::Closed);
}

/// CL-6: the desktop's `program_start` (an entry of the program library by its name, on the desktop's control
/// socket). Under an assistant it asks Georg first; alone it runs (the client asks him) unless Georg turned
/// `ai.mcp.ask_tools` off, then it is refused like every tool that changes something.
#[test]
fn starting_a_program_on_the_desktop_asks_first_and_alone_is_refused() {
    let w = World::new("mcp-desktop");
    let path = w.root.join("wayland-5.ctl");
    let l = UnixListener::bind(&path).unwrap();
    let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
    let heard = seen.clone();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let mut s = s.unwrap();
            let mut line = String::new();
            BufReader::new(&mut s).read_line(&mut line).unwrap();
            let line = line.trim().to_string();
            // The desktop's lookup: a word its library knows means one entry.
            let answer = match line.as_str() {
                "program-find Browser" | "program-find Brave" => "brave\tBrave".to_string(),
                "program-find Terminal" => "terminal\tTerminal".to_string(),
                l if l.starts_with("program-find ") => "error: no program of the library fits".to_string(),
                _ => "started Brave (pid 77); its window appears when the program has drawn it".to_string(),
            };
            heard.lock().unwrap().push(line);
            s.write_all(format!("{answer}\n").as_bytes()).unwrap();
        }
    });
    let above = Above { asked: Mutex::new(Vec::new()), answer: Mutex::new(Reply::Yes) };
    let asked = || std::mem::take(&mut *above.asked.lock().unwrap());
    serve_on_desktop(&w, None, Some(&path), open(), Some(&above), |c| {
        assert!(c.names().contains(&"program_start".to_string()));
        // The card names the entry that starts, not the model's word for it.
        let (said, is_error) = c.call("program_start", r#"{"name":"Browser"}"#).unwrap();
        assert!(!is_error && said.starts_with("started Brave (pid 77)"), "{said}");
        assert_eq!(asked(), [("program_start".to_string(), "Programm starten: Brave (brave)".to_string())]);
        *above.answer.lock().unwrap() = Reply::No(No::Declined);
        let (said, is_error) = c.call("program_start", r#"{"name":"Terminal"}"#).unwrap();
        assert!(is_error && said.contains("Georg hat Nein gesagt"), "{said}");
        // Not a command line: what is no name is refused before anybody is asked.
        assert!(!matches!(c.call("program_start", r#"{"name":"Brave","argv":["--evil"]}"#), Ok((_, false))));
        // A name that means no entry: nobody is asked.
        let (said, is_error) = c.call("program_start", r#"{"name":"Gibtsnicht"}"#).unwrap();
        assert!(is_error && said.contains("no program"), "{said}");
        assert_eq!(asked().len(), 1, "the no above was the only question");
    });
    // Alone, by default (ai.mcp.ask_tools on): the program at `assist --mcp` asks Georg itself, BitsOS does not.
    serve_on_desktop(&w, None, Some(&path), open(), None, |c| {
        let (said, is_error) = c.call("program_start", r#"{"name":"Brave"}"#).unwrap();
        assert!(!is_error && said.starts_with("started Brave (pid 77)"), "{said}");
    });
    // Alone, with ai.mcp.ask_tools turned off by Georg: refused like every tool that changes something.
    w.set("ai.mcp.ask_tools", false);
    serve_on_desktop(&w, None, Some(&path), open(), None, |c| {
        let (said, is_error) = c.call("program_start", r#"{"name":"Brave"}"#).unwrap();
        assert!(is_error && said.contains("ai.mcp.ask_tools"), "{said}");
    });
    assert_eq!(
        *seen.lock().unwrap(),
        ["program-find Browser", "program-start brave", "program-find Terminal", "program-find Gibtsnicht", "program-find Brave", "program-start brave", "program-find Brave"],
        "only a yes (Georg's, or the client's own question to him) started anything, by the id of the entry"
    );
}
