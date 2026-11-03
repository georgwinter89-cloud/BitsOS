//! The MCP wire protocol, shared by the kernel's MCP server (`crates/mcp`) and the user-space MCP service
//! `bitsmcp` (MC-8a, user/services/bitsmcp), so both answer byte for byte alike: the HTTP/1.1 head (`http`),
//! JSON (`json`), base64 (`b64`), the argument check (`schema`), JSON-RPC with the MCP methods over any tool set
//! (`rpc`), and the gate protocol of `/dev/bitsos/mcp` with the table of what the service offers (`gate`); MC-10:
//! the full-access grants and their clock rule (`access`).
//! Pure code over byte slices and `alloc`: no kernel crate, no `std` outside tests.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod access;
pub mod b64;
pub mod gate;
pub mod http;
pub mod installer;
pub mod json;
pub mod rpc;
pub mod schema;

use alloc::string::String;
use alloc::vec::Vec;
pub use json::Json;

/// A tool's safety class (B5): `read` looks only, `change` changes state, `delete_overwrite` destroys or replaces
/// data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Safety {
    Read,
    Change,
    DeleteOverwrite,
}

#[derive(Clone, Debug)]
pub enum ToolOutput {
    Text(String),
    /// PNG bytes (the server base64-encodes them as MCP image content).
    Png(Vec<u8>),
    Json(Json),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolError {
    /// A bad or missing argument (JSON-RPC -32602 with the argument's name).
    BadArgument(String),
    /// Refused by a safety rule or Georg's policy.
    Refused(String),
    Failed(String),
}

pub type ToolResult = Result<Vec<ToolOutput>, ToolError>;
