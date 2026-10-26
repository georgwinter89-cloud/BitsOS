//! settingsd (CL-1, V2-PLAN §8a.3): the settings registry of BitsOS V2. Typed keys with German and English
//! text, files on the stick (`\BITSOS2\settings\`) or a RAM fallback, a journal with undo, kernel keys bridged
//! through `/sys/bitsos/` and `CONFIG.TXT`, a JSON-lines socket API. The R8 safety switches are never keys.
#![forbid(unsafe_code)]

pub mod builtin;
pub mod change;
pub mod client;
pub mod events;
pub mod guard;
pub mod journal;
pub mod json;
pub mod key;
pub mod mcp;
pub mod mcp_tools;
pub mod proto;
pub mod registry;
pub mod server;
pub mod store;
mod migrate;
mod sync;
pub mod tomlish;
pub mod value;
