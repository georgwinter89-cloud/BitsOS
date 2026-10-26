//! Wanted permissions (B3): what a package asks for beyond its own package and data folder, in the kinds of
//! crates/policy's `Access`. bitspkg only checks and lists them; permd (PM) turns them into Georg's rules.

use crate::manifest::{Fields, abs_path};
use crate::names::check_name;
use crate::toml::Value;

/// crates/policy/src/lib.rs `DeviceClass`, in lower snake case.
pub const DEVICE_CLASSES: [&str; 9] =
    ["microphone", "audio", "camera", "input_grab", "screen_capture", "raw_usb", "disk", "gpu", "other"];

#[derive(Clone, Debug, PartialEq)]
pub enum Want {
    Path { path: String, write: bool, create: bool },
    /// `host` is a DNS name for the connection view, absent = any; `port` absent = any.
    NetConnect { proto: String, host: Option<String>, port: Option<u16> },
    NetBind { proto: String, port: u16 },
    NetListen { port: u16 },
    Device { class: String, name: Option<String> },
    /// Signals to the processes of another package.
    Signal { program: String },
    McpAction { tool: String },
    Scheduling { grant: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Permission {
    pub want: Want,
    /// Shown to Georg when he decides.
    pub why: String,
}

fn port(f: &Fields, n: Option<i64>, need: bool) -> Result<Option<u16>, String> {
    match n {
        None if need => f.wrap(Err("port missing".into())),
        None => Ok(None),
        Some(p) => u16::try_from(p).map(Some).or_else(|_| f.wrap(Err(format!("port {p} out of range")))),
    }
}

fn proto(f: &mut Fields) -> Result<String, String> {
    let p = f.str("proto")?;
    if matches!(p.as_str(), "tcp" | "udp") { Ok(p) } else { f.wrap(Err(format!("proto {p:?}: want tcp|udp"))) }
}

fn short_text(f: &Fields, what: &str, s: &str) -> Result<(), String> {
    if s.is_empty() || s.len() > 255 || s.chars().any(char::is_control) {
        return f.wrap(Err(format!("{what} {s:?} must be 1..255 bytes without control characters")));
    }
    Ok(())
}

impl Permission {
    pub(crate) fn from_fields(f: &mut Fields) -> Result<Permission, String> {
        let kind = f.str("kind")?;
        let why = f.opt_str("why")?.unwrap_or_default();
        let want = match kind.as_str() {
            "path" => {
                let path = f.str("path")?;
                abs_path(f, &path)?;
                let (write, create) = (f.bool("write")?, f.bool("create")?);
                if (write || create) && (path == "/" || path == "/apps" || path.starts_with("/apps/")) {
                    return f.wrap(Err(format!("{path}: the package store is read-only for programs")));
                }
                Want::Path { path, write, create }
            }
            "net_connect" => {
                let proto = proto(f)?;
                let host = f.opt_str("host")?;
                if let Some(h) = &host {
                    short_text(f, "host", h)?;
                }
                let p = f.opt_int("port")?;
                Want::NetConnect { proto, host, port: port(f, p, false)? }
            }
            "net_bind" => {
                let proto = proto(f)?;
                let p = f.opt_int("port")?;
                Want::NetBind { proto, port: port(f, p, true)?.unwrap_or(0) }
            }
            "net_listen" => {
                let p = f.opt_int("port")?;
                Want::NetListen { port: port(f, p, true)?.unwrap_or(0) }
            }
            "device" => {
                let class = f.str("class")?;
                if !DEVICE_CLASSES.contains(&class.as_str()) {
                    return f.wrap(Err(format!("device class {class:?}: want one of {}", DEVICE_CLASSES.join("|"))));
                }
                let name = f.opt_str("name")?;
                if let Some(n) = &name {
                    short_text(f, "device name", n)?;
                }
                Want::Device { class, name }
            }
            "signal" => {
                let program = f.str("program")?;
                f.wrap(check_name(&program))?;
                Want::Signal { program }
            }
            "mcp_action" => {
                let tool = f.str("tool")?;
                short_text(f, "tool", &tool)?;
                Want::McpAction { tool }
            }
            "scheduling" => {
                let grant = f.str("grant")?;
                short_text(f, "grant", &grant)?;
                Want::Scheduling { grant }
            }
            _ => return f.wrap(Err(format!("permission kind {kind:?} unknown"))),
        };
        Ok(Permission { want, why })
    }

    pub fn kind(&self) -> &'static str {
        match self.want {
            Want::Path { .. } => "path",
            Want::NetConnect { .. } => "net_connect",
            Want::NetBind { .. } => "net_bind",
            Want::NetListen { .. } => "net_listen",
            Want::Device { .. } => "device",
            Want::Signal { .. } => "signal",
            Want::McpAction { .. } => "mcp_action",
            Want::Scheduling { .. } => "scheduling",
        }
    }

    pub(crate) fn to_entries(&self) -> Vec<(&'static str, Value)> {
        let s = |v: &str| Value::Str(v.to_string());
        let mut v = vec![("kind", s(self.kind()))];
        match &self.want {
            Want::Path { path, write, create } => {
                v.push(("path", s(path)));
                v.push(("write", Value::Bool(*write)));
                v.push(("create", Value::Bool(*create)));
            }
            Want::NetConnect { proto, host, port } => {
                v.push(("proto", s(proto)));
                if let Some(h) = host {
                    v.push(("host", s(h)));
                }
                if let Some(p) = port {
                    v.push(("port", Value::Int(i64::from(*p))));
                }
            }
            Want::NetBind { proto, port } => {
                v.push(("proto", s(proto)));
                v.push(("port", Value::Int(i64::from(*port))));
            }
            Want::NetListen { port } => v.push(("port", Value::Int(i64::from(*port)))),
            Want::Device { class, name } => {
                v.push(("class", s(class)));
                if let Some(n) = name {
                    v.push(("name", s(n)));
                }
            }
            Want::Signal { program } => v.push(("program", s(program))),
            Want::McpAction { tool } => v.push(("tool", s(tool))),
            Want::Scheduling { grant } => v.push(("grant", s(grant))),
        }
        if !self.why.is_empty() {
            v.push(("why", s(&self.why)));
        }
        v
    }

    /// One line for `bitspkg info`.
    pub fn describe(&self) -> String {
        let e: Vec<String> = self.to_entries().iter().skip(1).map(|(k, v)| format!("{k}={}", crate::toml::write_value(v))).collect();
        format!("{} {}", self.kind(), e.join(" "))
    }
}
