//! The generic kernel tools of MC-2 (`V2-PLAN.md` §6, `spec/net.md` §5.2): `hw_info`, `net_info`, `ktest`,
//! `cpus`, `reboot`, `msr_read`, `read_phys`; MC-4a's file tools and `fetch` (`files`). Tools whose back end belongs to another lane are registered by that
//! lane in its own crate (`log` by RB, `pci_read` by PC, ...). There is no `poweroff` tool (R8f).

mod cpus;
/// MC-4a: read_file, write_file, list_dir, stat, delete, fetch on the VFS.
pub mod files;
mod hw_info;
pub mod input;
/// MC-4b: install_kernel (RB-6's tool) over MCP.
#[cfg(target_os = "none")]
mod install_ktests;
pub(crate) mod ktests;
mod ktests_mc3;
mod msr;
mod net_info;
/// PC's tools (pci_read/pci_write/mmio_read/mmio_write/pci_rescan), in a PC-owned file (CONTRACTS 6a).
pub(crate) mod pc;
mod phys;
mod reboot;
mod run_ktests;
pub mod screen;

pub use phys::{PHYS_MAX, phys_check};
pub use reboot::ResetPlan;
pub use run_ktests::{Verdict, verdict};

use crate::{Json, ToolError, ToolOutput, ToolResult};
use alloc::string::String;
use alloc::vec::Vec;

/// Malformed and hostile arguments: every one must be answered with invalid params (-32602) or a refusal, never
/// a panic (host test in this file, ktest `mcp.tool.hostile_args`). No `reboot` here: a broken check would restart
/// the machine the ktest runs on (its argument check is host-tested).
pub(crate) const HOSTILE: &[(&str, &str)] = &[
    ("read_phys", "{}"),
    ("read_phys", r#"{"addr":null}"#),
    ("read_phys", r#"{"addr":-1}"#),
    ("read_phys", r#"{"addr":"-1"}"#),
    ("read_phys", r#"{"addr":"zz"}"#),
    ("read_phys", r#"{"addr":"0x"}"#),
    ("read_phys", r#"{"addr":"  "}"#),
    ("read_phys", r#"{"addr":1e30}"#),
    ("read_phys", r#"{"addr":1.5}"#),
    ("read_phys", r#"{"addr":99999999999999999999999999}"#),
    ("read_phys", r#"{"addr":"0xFFFFFFFFFFFFFFFFF"}"#),
    ("read_phys", r#"{"addr":[4096]}"#),
    ("read_phys", r#"{"addr":{"x":1}}"#),
    ("read_phys", r#"{"addr":true}"#),
    ("read_phys", r#"{"addr":4096,"len":"x"}"#),
    ("read_phys", r#"{"addr":4096,"len":-5}"#),
    ("read_phys", r#"{"addr":4096,"len":0}"#),
    ("read_phys", r#"{"addr":4096,"len":18446744073709551615}"#),
    ("read_phys", r#"{"addr":"0xFFFFFFFFFFFFFFF0","len":4096}"#),
    ("read_phys", r#"{"addr":4096,"format":"bogus"}"#),
    ("read_phys", r#"{"addr":4096,"format":7}"#),
    ("read_phys", "[1]"),
    ("read_phys", r#""text""#),
    ("msr_read", "{}"),
    ("msr_read", r#"{"msr":null}"#),
    ("msr_read", r#"{"msr":-1}"#),
    ("msr_read", r#"{"msr":"zz"}"#),
    ("msr_read", r#"{"msr":1e30}"#),
    ("msr_read", r#"{"msr":true}"#),
    ("msr_read", r#"{"msr":{}}"#),
    ("msr_read", r#"{"msr":"0x1_0000_0000"}"#),
    ("msr_read", r#"{"msr":4294967296}"#),
    ("msr_read", r#"{"msr":"0xFFFFFFFF"}"#),
    ("ktest", r#"{"filter":5}"#),
    ("ktest", r#"{"filter":["a"]}"#),
    ("ktest", r#"{"filter":"a\u0001b"}"#),
    ("hw_info", r#"{"section":7}"#),
    ("hw_info", r#"{"section":"../../etc"}"#),
    ("net_info", r#"{"connections":-1}"#),
    ("net_info", r#"{"connections":"many"}"#),
    ("output", r#"{"call":-3}"#),
    ("output", r#"{"call":"x"}"#),
    ("stop", r#"{"call":1e30}"#),
    ("stop", r#"{"call":987654321}"#),
    ("screen", r#"{"format":"bmp"}"#),
    ("screen", r#"{"format":7}"#),
    ("screen", r#"{"scale":0}"#),
    ("screen", r#"{"scale":17}"#),
    ("screen", r#"{"scale":1e30}"#),
    ("screen", r#"{"x":-1}"#),
    ("screen", r#"{"x":99999999}"#),
    ("screen", r#"{"width":"wide"}"#),
    ("screen", r#"{"width":0}"#),
    ("screen", r#"{"format":"text","scale":2}"#),
    // None of these may send anything: each is refused before the first event.
    ("send_keys", "{}"),
    ("send_keys", r#"{"text":5}"#),
    ("send_keys", r#"{"text":"a\u0000"}"#),
    ("send_keys", r#"{"text":"日本"}"#),
    ("send_keys", r#"{"keys":"enter"}"#),
    ("send_keys", r#"{"keys":[null]}"#),
    ("send_keys", r#"{"keys":["enter","ctrl+"]}"#),
    ("send_keys", r#"{"layout":"xx","text":"a"}"#),
    ("send_keys", r#"{"text":"a","injected":false}"#),
    ("send_mouse", "{}"),
    ("send_mouse", r#"{"dx":1e30}"#),
    ("send_mouse", r#"{"dx":70000}"#),
    ("send_mouse", r#"{"x":-5,"y":1}"#),
    ("send_mouse", r#"{"x":5}"#),
    ("send_mouse", r#"{"action":"click"}"#),
    ("send_mouse", r#"{"button":"left","injected":false}"#),
    // MC-4a: none of these may change a file or open a connection.
    ("read_file", "{}"),
    ("read_file", r#"{"path":7}"#),
    ("read_file", r#"{"path":"relative"}"#),
    ("read_file", r#"{"path":"/dev/null"}"#),
    ("read_file", r#"{"path":"/","length":99999999}"#),
    ("read_file", r#"{"path":"/x\u0000y"}"#),
    ("write_file", r#"{"path":"/dev/null","text":"x"}"#),
    ("write_file", r#"{"path":"/proc/mc4a-hostile","text":"x"}"#),
    ("write_file", r#"{"path":"/","text":"x"}"#),
    ("write_file", r#"{"path":"/tmp/mc4a-hostile","data":"%%%"}"#),
    ("write_file", r#"{"path":"/tmp/mc4a-hostile","text":"x","offset":-4}"#),
    ("write_file", r#"{"path":"/tmp/mc4a-hostile","text":"x","mode":"rwx"}"#),
    ("write_file", r#"{"path":"/tmp/CONFIG.TXT","text":"nvme_write=on"}"#),
    ("list_dir", r#"{"path":["/"]}"#),
    ("list_dir", r#"{"path":"/no/such/dir/mc4a"}"#),
    ("stat", r#"{"path":"/dev/null","hash":true}"#),
    ("stat", r#"{"path":"/","follow":"no"}"#),
    ("delete", "{}"),
    ("delete", r#"{"path":"/"}"#),
    ("delete", r#"{"path":"/tmp"}"#),
    ("delete", r#"{"path":"/dev/null"}"#),
    ("delete", r#"{"path":"/tmp/.."}"#),
    ("delete", r#"{"path":"/tmp","recursive":"yes"}"#),
    ("fetch", "{}"),
    ("fetch", r#"{"url":"http://8.8.8.8/x","path":"/tmp/mc4a-hostile"}"#),
    ("fetch", r#"{"url":"https://192.168.1.1/x","path":"/tmp/mc4a-hostile"}"#),
    ("fetch", r#"{"url":"http://bitsos.fritz.box/x","path":"/tmp/mc4a-hostile"}"#),
    ("fetch", r#"{"url":"http://10.0.2.2/x","path":"/dev/mc4a-hostile"}"#),
    ("fetch", r#"{"url":"file:///etc/passwd","path":"/tmp/mc4a-hostile"}"#),
    // MC-push: manifest and move_tree (every one refused or failing before anything moves).
    ("manifest", "{}"),
    ("manifest", r#"{"path":5}"#),
    ("manifest", r#"{"path":"relative"}"#),
    ("manifest", r#"{"path":"/","hash":"md5"}"#),
    ("manifest", r#"{"path":"/","after":7}"#),
    ("manifest", r#"{"path":"/","missing_ok":"yes"}"#),
    ("manifest", r#"{"path":"/dev/null"}"#),
    ("move_tree", "{}"),
    ("move_tree", r#"{"from":"/tmp/mcpush-hostile-none","to":"/dev/x"}"#),
    ("move_tree", r#"{"from":"/proc","to":"/tmp/mcpush-hostile"}"#),
    ("move_tree", r#"{"from":"/tmp","to":"/tmp/mcpush-hostile"}"#),
    ("move_tree", r#"{"from":"/tmp/mcpush-hostile-none","to":"/tmp/mcpush-hostile","remove":["../etc"]}"#),
    ("move_tree", r#"{"from":"/tmp/mcpush-hostile-none","to":"/tmp/mcpush-hostile","remove":{"a":1}}"#),
    ("move_tree", r#"{"from":"/tmp/mcpush-hostile-none","to":"/boot/BITSOS2/SYS/9"}"#),
    ("move_tree", r#"{"from":"/boot/BITSOS2/SYS","to":"/tmp/mcpush-hostile"}"#),
    ("move_tree", r#"{"from":"/boot/BITSOS2/home","to":"/boot/BITSOS2/mcpush-hostile-home"}"#),
    ("move_tree", r#"{"from":"/boot/bitsos2/HOME.","to":"/tmp/mcpush-hostile"}"#),
    ("manifest", r#"{"path":"/tmp","only":"../etc"}"#),
    ("manifest", r#"{"path":"/tmp","only":"/etc"}"#),
    // MC-4b: install_kernel (RB's tool; not registered in the host build, where these are unknown tools): every
    // one is refused before anything is fetched or the stick is touched.
    ("install_kernel", "{}"),
    ("install_kernel", r#"{"url":5}"#),
    ("install_kernel", r#"{"url":null,"initrd":"http://10.0.2.2/b/INITRD"}"#),
    ("install_kernel", r#"{"url":"http://8.8.8.8/b/"}"#),
    ("install_kernel", r#"{"url":"https://192.168.1.1/b/"}"#),
    ("install_kernel", r#"{"url":"file:///boot/BITSOS2/SYS/1/"}"#),
    ("install_kernel", r#"{"url":"http://bitsos.fritz.box/b/"}"#),
    ("install_kernel", r#"{"url":"http://10.0.2.2/b/KERNEL"}"#),
    ("install_kernel", r#"{"url":"http://10.0.2.2/b/","dry_run":"yes"}"#),
    ("install_kernel", r#"{"url":"http://10.0.2.2/b/","config":7}"#),
    ("install_kernel", r#"{"url":"http://10.0.2.2/b/","unpin":1}"#),
    ("install_kernel", r#"{"url":"http://10.0.2.2/b/","allow_no_fallback":"no"}"#),
    ("install_kernel", r#"{"kernel_path":"/tmp/mc4b-hostile/KERNEL"}"#),
    ("install_kernel", "[1]"),
    // RB-9: system_pin and install_system (RB's tools; unknown in the host build): each refused before the stick
    // or the network is touched (rb.install.system_list checks the same set against the handler).
    ("system_pin", "{}"),
    ("system_pin", r#"{"n":"x"}"#),
    ("system_pin", r#"{"n":-1}"#),
    ("system_pin", r#"{"n":1.5}"#),
    ("system_pin", r#"{"n":0}"#),
    ("system_pin", r#"{"n":100000000}"#),
    ("system_pin", r#"{"n":"OFF"}"#),
    ("system_pin", r#"{"n":1,"now":"yes"}"#),
    ("system_pin", r#"{"n":1,"dry_run":1}"#),
    ("system_pin", "[1]"),
    ("install_system", "{}"),
    ("install_system", r#"{"url":5}"#),
    ("install_system", r#"{"url":"http://8.8.8.8/b/"}"#),
    ("install_system", r#"{"url":"https://192.168.1.1/b/"}"#),
    ("install_system", r#"{"url":"http://10.0.2.2/b/KERNEL"}"#),
    ("install_system", r#"{"url":"http://10.0.2.2/b/","dry_run":"yes"}"#),
    ("install_system", "[1]"),
    // IN-6's `latency` (usb-hid; usb-hid's host test tool_latency checks the same set against the handler).
    ("latency", r#"{"last":-1}"#),
    ("latency", r#"{"last":"x"}"#),
    ("latency", r#"{"last":5000}"#),
    ("latency", r#"{"last":1.5}"#),
    ("latency", r#"{"keep":0}"#),
    ("latency", r#"{"keep":5000}"#),
    ("latency", r#"{"keep":1.5}"#),
    ("latency", r#"{"reset":"yes"}"#),
    ("latency", r#"{"reset":1}"#),
];

pub(crate) fn bad(name: &str) -> ToolError {
    ToolError::BadArgument(String::from(name))
}

/// A number given as a JSON integer or as text ("0xC0000080", "4096").
pub(crate) fn number(args: &Json, key: &str) -> Result<Option<u64>, ToolError> {
    match args.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Str(s)) => {
            let t = s.trim();
            let v = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                Some(h) => u64::from_str_radix(&h.replace('_', ""), 16),
                None => t.replace('_', "").parse::<u64>(),
            };
            v.map(Some).map_err(|_| bad(key))
        }
        Some(v) => v.as_u64().map(Some).ok_or_else(|| bad(key)),
    }
}

pub(crate) fn opt_str<'a>(args: &'a Json, key: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => v.as_str().map(Some).ok_or_else(|| bad(key)),
    }
}

pub(crate) fn opt_bool(args: &Json, key: &str) -> Result<Option<bool>, ToolError> {
    match args.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => v.as_bool().map(Some).ok_or_else(|| bad(key)),
    }
}

pub(crate) fn text(lines: Vec<String>) -> ToolResult {
    Ok(alloc::vec![ToolOutput::Text(lines.join("\n"))])
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::{Safety, TOOLS, rpc};
    use std::string::ToString;

    fn call(name: &str, args: &str) -> Json {
        let body = alloc::format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#);
        let a = rpc::handle(body.as_bytes(), 1_000_000_000, TOOLS.entries());
        Json::parse(&a.body).unwrap()
    }
    fn text_of(j: &Json) -> String {
        let r = j.get("result").unwrap_or_else(|| panic!("no result: {}", j.to_string()));
        r.get("content").and_then(Json::as_array).and_then(|c| c.first()).and_then(|c| c.get("text")).and_then(Json::as_str).unwrap_or("").to_string()
    }
    fn failed(j: &Json) -> bool {
        j.get("result").and_then(|r| r.get("isError")).and_then(Json::as_bool) == Some(true)
    }
    fn code(j: &Json) -> Option<i64> {
        j.get("error").and_then(|e| e.get("code")).and_then(Json::as_i64)
    }

    #[test]
    fn the_generic_tools_are_registered_and_there_is_no_poweroff() {
        let want = [
            ("cpus", Safety::Read), ("hw_info", Safety::Read), ("ktest", Safety::Change), ("msr_read", Safety::Read), ("net_info", Safety::Read),
            ("output", Safety::Read), ("read_phys", Safety::Read), ("reboot", Safety::Change), ("stop", Safety::Change), ("version", Safety::Read),
            ("screen", Safety::Read), ("send_keys", Safety::Change), ("send_mouse", Safety::Change),
            ("read_file", Safety::Read), ("list_dir", Safety::Read), ("stat", Safety::Read), ("write_file", Safety::Change),
            ("delete", Safety::DeleteOverwrite), ("fetch", Safety::Change), ("manifest", Safety::Read), ("move_tree", Safety::DeleteOverwrite),
        ];
        for (name, safety) in want {
            let t = crate::find(name).unwrap_or_else(|| panic!("{name} missing"));
            assert_eq!((t.owner, t.safety), ("MC", safety), "{name}");
            assert!(Json::parse(t.input_schema).is_ok(), "{name}");
        }
        for t in TOOLS.entries() {
            let n = t.name.to_ascii_lowercase();
            assert!(!n.contains("poweroff") && !n.contains("power_off") && !n.contains("shutdown"), "R8f: {}", t.name);
        }
    }

    #[test]
    fn hw_info_sections_on_the_host() {
        let all = text_of(&call("hw_info", "{}"));
        for s in ["kernel", "cpu", "memory", "firmware", "interrupts", "msi", "pci"] {
            assert!(all.contains(&alloc::format!("== {s} ==")), "{s}: {all}");
        }
        assert!(all.contains("cpu vendor=") && all.contains("next start: unknown"), "{all}");
        let one = text_of(&call("hw_info", r#"{"section":"memory"}"#));
        assert!(one.starts_with("== memory ==") && !one.contains("== cpu =="), "{one}");
        assert_eq!(code(&call("hw_info", r#"{"section":"disk"}"#)), Some(-32602));
    }

    #[test]
    fn refusals_without_hardware() {
        let j = call("read_phys", r#"{"addr":"0x100000","len":16}"#);
        assert!(failed(&j) && text_of(&j).starts_with("Refused: no firmware memory map"), "{}", j.to_string());
        assert_eq!(code(&call("read_phys", r#"{"addr":"0xZZ"}"#)), Some(-32602));
        assert_eq!(code(&call("read_phys", r#"{}"#)), Some(-32602), "addr is required");
        assert_eq!(code(&call("read_phys", r#"{"addr":4096,"format":"octal"}"#)), Some(-32602));
        let j = call("msr_read", r#"{"msr":"0x10"}"#);
        assert!(failed(&j) && text_of(&j).contains("not on the read list"), "TSC is not on the list: {}", j.to_string());
        assert!(!failed(&call("msr_read", r#"{"msr":"0xC0000080"}"#)), "EFER is");
        assert_eq!(code(&call("msr_read", r#"{"msr":"0x1_0000_0000"}"#)), Some(-32602), "beyond 32 bits");
        let j = call("ktest", r#"{"filter":"zz.no.such.test"}"#);
        assert!(failed(&j) && text_of(&j).contains("no ktest matches"), "{}", j.to_string());
    }

    #[test]
    fn hostile_arguments_are_answered_not_panicked_on() {
        let extra = [("reboot", r#"{"dry_run":"yes"}"#), ("reboot", r#"{"dry_run":1}"#), ("reboot", "[true]")];
        for (tool, args) in HOSTILE.iter().chain(extra.iter()) {
            let j = call(tool, args);
            let refused = failed(&j) && (text_of(&j).starts_with("Refused: ") || text_of(&j).starts_with("ERROR: "));
            assert!(code(&j) == Some(-32602) || refused, "{tool} {args}: {}", j.to_string());
        }
    }

    #[test]
    fn reboot_answers_first_or_not_at_all() {
        let j = call("reboot", r#"{"dry_run":true}"#);
        let t = text_of(&j);
        assert!(!failed(&j) && t.starts_with("reboot (dry run") && t.contains("next start: unknown") && t.contains("keyboard controller"), "{t}");
        // Without a FADT the keyboard controller is only a guess: a real reboot needs force.
        let j = call("reboot", "{}");
        assert!(failed(&j) && text_of(&j).contains("force:true tries them anyway"), "{}", j.to_string());
        // Outside a server there is no answer to wait for: the tool refuses to restart at all.
        let j = call("reboot", r#"{"force":true}"#);
        assert!(failed(&j) && text_of(&j).contains("only after its answer"), "{}", j.to_string());
    }

    #[test]
    fn net_info_on_the_host() {
        let t = text_of(&call("net_info", r#"{"connections":3}"#));
        assert!(t.contains("== interfaces ==") && t.contains("== counters ==") && t.contains("== connections (newest 3) =="), "{t}");
        assert_eq!(code(&call("net_info", r#"{"connections":5000}"#)), Some(-32602));
    }

    #[test]
    fn cpus_on_the_host() {
        let t = text_of(&call("cpus", "{}"));
        assert!(t.starts_with("cpus online=") && t.contains("threads n="), "{t}");
    }

    #[test]
    fn numbers_as_integer_or_text() {
        let a = Json::parse(r#"{"a":4096,"b":"0xC000_0080","c":"17","d":"x1","e":-1,"f":1.5,"g":null}"#).unwrap();
        assert_eq!(number(&a, "a"), Ok(Some(4096)));
        assert_eq!(number(&a, "b"), Ok(Some(0xC000_0080)));
        assert_eq!(number(&a, "c"), Ok(Some(17)));
        assert_eq!(number(&a, "d"), Err(bad("d")));
        assert_eq!(number(&a, "e"), Err(bad("e")));
        assert_eq!(number(&a, "f"), Err(bad("f")));
        assert_eq!(number(&a, "g"), Ok(None));
        assert_eq!(number(&a, "missing"), Ok(None));
    }
}
