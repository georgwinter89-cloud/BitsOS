//! The service's catalog and runner: the gate's `list` (fetched for each request, so service tools that come
//! and go are seen as the kernel server sees them), kernel tools the table drops (answered with their reason
//! instead of "Unknown tool"), calls through `call`, installer requests through `install`.
//!
//! MC-10: the list and every call name the client's key (`client`, the TLS certificate's SHA-256; "" for a plain
//! connection), so a client with a live full-access grant gets every kernel tool; the service's own tools
//! `request_access` (a question for Georg at the keyboard through the installer) and `access_status` are listed
//! for everyone and answered only for a TLS client.

use crate::link::{EPERM, Lease};
use mcp_wire::gate::{self, Listed, Request};
use mcp_wire::rpc::{self, Host, Info, ToolMeta, ToolSet};
use mcp_wire::{Json, Safety, ToolError, ToolResult};

pub const INSTRUCTIONS: &str = "BitsOS V2 is Georg's own operating system: a Rust kernel that runs Linux x86-64 programs. \
This MCP server is the user-space service bitsmcp: it reaches the kernel only through a narrow interface (hardware \
facts, metrics, events and the journal, Safe-class ktests); tools that act (install, Reload, pin, reboot) go to the \
installer, which asks a person first. A paired client (mutual TLS) can ask Georg for full access with request_access: \
he chooses the time at the Mini-PC's real keyboard, and until then every kernel tool (run, files, screen, send_keys, \
installs, reboot, every ktest class) is offered to that client; access_status tells the time left. tools/list names \
each tool with its owner (the lane that built it) and safety class (read, change, delete_overwrite) in its \
annotations. Calls take an optional timeout_ms (1 to 86400000, default 10000); a call still running then answers \
'Still running ... (call n)', and output shows its progress and, once it ends, its result; stop asks it to end. \
Tools whose owner is a lower-case service name come from BitsOS's user-space services (annotation service_pid): \
they appear and vanish with their service.";

pub const INFO: Info<'static> = Info { version: env!("CARGO_PKG_VERSION"), instructions: INSTRUCTIONS };

/// A tool the service knows of.
#[derive(Clone, Debug)]
pub enum Entry {
    Offered(Listed),
    /// A kernel tool the table drops, with the reason.
    Dropped(String, &'static str),
    /// MC-10: the service's own `request_access` / `access_status`.
    Own(&'static Listed),
}

fn own(name: &'static str, description: &'static str, schema: &'static str, safety: Safety) -> Listed {
    Listed { name: name.into(), description: description.into(), schema: schema.into(), owner: "MC".into(), safety, verified: true, pid: None, service: false, installer: false }
}

/// MC-10: the service's own tools, after the kernel's.
pub fn own_tools() -> &'static [Listed] {
    static OWN: std::sync::OnceLock<Vec<Listed>> = std::sync::OnceLock::new();
    OWN.get_or_init(|| {
        use mcp_wire::access as acc;
        vec![
            own(acc::ACCESS_STATUS, acc::ACCESS_STATUS_DESCRIPTION, acc::ACCESS_STATUS_SCHEMA, Safety::Read),
            own(acc::REQUEST_ACCESS, acc::REQUEST_ACCESS_DESCRIPTION, acc::REQUEST_ACCESS_SCHEMA, Safety::Change),
        ]
    })
}

impl ToolMeta for Entry {
    fn name(&self) -> &str {
        match self {
            Entry::Offered(l) => &l.name,
            Entry::Own(l) => &l.name,
            Entry::Dropped(n, _) => n,
        }
    }
    fn description(&self) -> &str {
        match self {
            Entry::Offered(l) => &l.description,
            Entry::Own(l) => &l.description,
            Entry::Dropped(_, why) => why,
        }
    }
    fn input_schema(&self) -> &str {
        match self {
            Entry::Offered(l) => &l.schema,
            Entry::Own(l) => &l.schema,
            Entry::Dropped(..) => r#"{"type":"object"}"#,
        }
    }
    fn owner(&self) -> &str {
        match self {
            Entry::Offered(l) => &l.owner,
            Entry::Own(l) => &l.owner,
            Entry::Dropped(..) => "kernel",
        }
    }
    fn safety(&self) -> Safety {
        match self {
            Entry::Offered(l) => l.safety,
            Entry::Own(l) => l.safety,
            Entry::Dropped(..) => Safety::Read,
        }
    }
    fn service_pid(&self) -> Option<u32> {
        match self {
            Entry::Offered(l) => l.pid,
            Entry::Own(_) | Entry::Dropped(..) => None,
        }
    }
    fn safety_verified(&self) -> bool {
        match self {
            Entry::Offered(l) => l.verified,
            Entry::Own(_) | Entry::Dropped(..) => true,
        }
    }
    fn is_service(&self) -> bool {
        matches!(self, Entry::Offered(l) if l.service)
    }
}

/// The tools of one request.
pub struct Catalog(pub Vec<Listed>);

impl Catalog {
    /// The gate's list for `client` ("" for a plain connection); None when the kernel did not answer it.
    pub fn fetch(link: &mut Lease<'_>, client: &str) -> Option<Catalog> {
        let text = link.command(&gate::encode_list_for(client)).ok()?;
        gate::decode_list_answer(&text).map(Catalog)
    }
}

impl ToolSet for Catalog {
    type Tool = Entry;
    fn find(&self, name: &str) -> Option<Entry> {
        if let Some(l) = self.0.iter().find(|t| t.name == name) {
            return Some(Entry::Offered(l.clone()));
        }
        if let Some(l) = own_tools().iter().find(|t| t.name == name) {
            return Some(Entry::Own(l));
        }
        gate::known_dropped(name).map(|why| Entry::Dropped(String::from(name), why))
    }
    fn sorted(&self) -> Vec<Entry> {
        // The kernel sends them sorted by name (its catalog's order); dropped tools are not listed; the
        // service's own two come last.
        self.0.iter().cloned().map(Entry::Offered).chain(own_tools().iter().map(Entry::Own)).collect()
    }
}

/// The service around a call: the policy check is the kernel's (at the gate), events are the kernel's too.
pub struct ServiceHost;

impl Host for ServiceHost {
    fn bad_schema(&self, tool: &str, owner: &str) {
        crate::log::warn(&format!("tool {tool} (owner {owner}): its schema is not JSON; listed as an object"));
    }
}

/// Runs `tool` until `deadline_ns` (the service's clock) through the gate, for the client with key `client`.
pub fn run(link: &mut Lease<'_>, tool: &Entry, deadline_ns: u64, args: &Json, peer: &str, client: &str) -> Result<ToolResult, String> {
    let left_ms = deadline_ns.saturating_sub(crate::now_ns()).div_ceil(1_000_000).clamp(1, rpc::MAX_TIMEOUT_MS);
    let l = match tool {
        Entry::Dropped(name, why) => {
            crate::log::info(&format!("refused tool={name} peer={peer}: not offered by the service"));
            return Ok(Err(ToolError::Refused(gate::dropped_text(name, why))));
        }
        Entry::Own(l) => return own_run(link, &l.name, args, peer, client, left_ms),
        Entry::Offered(l) => l,
    };
    let r = Request { tool: l.name.clone(), args: args.clone(), timeout_ms: left_ms, peer: String::from(peer), client: String::from(client) };
    if l.installer {
        return match link.command(&gate::encode_install(&r)) {
            Err(errno) => Ok(Err(ToolError::Refused(gate::installer_text(&l.name, errno)))),
            Ok(text) => decoded(&text),
        };
    }
    match link.command(&gate::encode_call(&r)) {
        Ok(text) => decoded(&text),
        Err(EPERM) => Ok(Err(ToolError::Refused(format!("the kernel's gate does not offer {} (EPERM)", l.name)))),
        Err(errno) => Ok(Err(ToolError::Failed(format!("the kernel's gate answered errno {errno}")))),
    }
}

/// MC-10: `access_status` and `request_access`.
fn own_run(link: &mut Lease<'_>, name: &str, args: &Json, peer: &str, client: &str, left_ms: u64) -> Result<ToolResult, String> {
    use mcp_wire::access as acc;
    if client.is_empty() {
        return Ok(Err(ToolError::Refused(format!("{name} is only for a paired client over TLS (host/mcp/pair.mjs, then --tls)"))));
    }
    if name == acc::ACCESS_STATUS {
        return match link.command(&acc::encode_access(client)).ok().and_then(|t| acc::decode_status(&t)) {
            Some(st) => Ok(Ok(vec![mcp_wire::ToolOutput::Text(acc::status_text(&st)), mcp_wire::ToolOutput::Json(Json::obj(vec![
                ("granted", Json::Bool(st.granted)),
                ("until", Json::Num(st.until as f64)),
                ("left_s", Json::Num(st.left_s as f64)),
                ("nvme", Json::Bool(st.nvme)),
                ("loaded", Json::Bool(st.loaded)),
                ("local", Json::str(&st.local)),
            ]))])),
            None => Ok(Err(ToolError::Failed(String::from("the kernel's gate did not answer access")))),
        };
    }
    crate::log::info(&format!("request_access from {peer} (client {}): asking the installer", acc::fp_short(client)));
    let r = Request { tool: String::from(acc::REQUEST_ACCESS), args: args.clone(), timeout_ms: left_ms, peer: String::from(peer), client: String::from(client) };
    match link.command(&gate::encode_install(&r)) {
        Err(errno) => Ok(Err(ToolError::Refused(gate::installer_text(acc::REQUEST_ACCESS, errno)))),
        Ok(text) => decoded(&text),
    }
}

fn decoded(text: &str) -> Result<ToolResult, String> {
    gate::decode_result(text).unwrap_or_else(|| Ok(Err(ToolError::Failed(String::from("the kernel's answer was not understood")))))
}
