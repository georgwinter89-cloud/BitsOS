//! MC-6: the tools a server offers: the built-in registry (`mcp_tool!`, static) and, when the server has one, the
//! tools user-space services registered through `/run/bitsos/mcp.sock` (`svc`). A built-in name always wins; the
//! registry refuses a service tool with a built-in's name anyway.

use crate::svc::{Registry, SvcTool};
use crate::{Json, Safety, ToolContext, ToolEntry, ToolResult};
use alloc::sync::Arc;
use alloc::vec::Vec;

/// One tool, built in or registered by a service.
#[derive(Clone)]
pub enum Tool {
    Static(&'static ToolEntry),
    Service(Arc<SvcTool>),
}

impl Tool {
    pub fn name(&self) -> &str {
        match self {
            Tool::Static(t) => t.name,
            Tool::Service(t) => &t.name,
        }
    }
    pub fn description(&self) -> &str {
        match self {
            Tool::Static(t) => t.description,
            Tool::Service(t) => &t.description,
        }
    }
    /// The argument schema as JSON text.
    pub fn input_schema(&self) -> &str {
        match self {
            Tool::Static(t) => t.input_schema,
            Tool::Service(t) => &t.schema,
        }
    }
    /// The lane (built-in) or the service's name (B5).
    pub fn owner(&self) -> &str {
        match self {
            Tool::Static(t) => t.owner,
            Tool::Service(t) => &t.owner,
        }
    }
    pub fn safety(&self) -> Safety {
        match self {
            Tool::Static(t) => t.safety,
            Tool::Service(t) => t.safety,
        }
    }
    /// The pid of the service behind a registered tool.
    pub fn service_pid(&self) -> Option<u32> {
        match self {
            Tool::Static(_) => None,
            Tool::Service(t) => Some(t.pid),
        }
    }
    /// Whether the listed safety class is vouched for: built-in tools and services of uid 0 (MC-6).
    pub fn safety_verified(&self) -> bool {
        match self {
            Tool::Static(_) => true,
            Tool::Service(t) => t.uid == 0,
        }
    }
    pub fn is_service(&self) -> bool {
        matches!(self, Tool::Service(_))
    }
    /// Runs the handler (built in) or forwards the call to its service, bounded by `ctx.deadline_ns`.
    pub fn invoke(&self, ctx: &ToolContext, args: &Json) -> ToolResult {
        match self {
            Tool::Static(t) => (t.handler)(ctx, args),
            Tool::Service(t) => crate::svc::forward(t, ctx, args),
        }
    }
}

/// What `tools/list` and `tools/call` see.
#[derive(Clone, Copy)]
pub struct Catalog<'a> {
    pub statics: &'static [ToolEntry],
    pub services: Option<&'a Registry>,
}

impl<'a> Catalog<'a> {
    pub fn of(statics: &'static [ToolEntry]) -> Catalog<'static> {
        Catalog { statics, services: None }
    }
    pub fn find(&self, name: &str) -> Option<Tool> {
        if let Some(t) = self.statics.iter().find(|t| t.name == name) {
            return Some(Tool::Static(t));
        }
        self.services.and_then(|r| r.find(name)).map(Tool::Service)
    }
    /// Every tool, sorted by name (a snapshot: the registry's lock is held only to copy it).
    pub fn sorted(&self) -> Vec<Tool> {
        let mut v: Vec<Tool> = self.statics.iter().map(Tool::Static).collect();
        if let Some(r) = self.services {
            v.extend(r.snapshot().into_iter().map(Tool::Service));
        }
        v.sort_by(|a, b| a.name().cmp(b.name()));
        v
    }
}
