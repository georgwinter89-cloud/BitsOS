//! bitsaccess (DB2, the layer "See" of Durchblick): the kernel's access log (`/dev/bitsos/access`, vfs::access)
//! turned into what Georg reads on the page "Wer hat was gemacht": per program the folders it read and wrote, the
//! hosts it talked to, the devices it opened and the rights it raised, 30 days back, all on this machine.
//! - `kernel`: the node's JSON lines -> entries with wall times and program paths;
//! - `model`: an entry and how it is named (device classes, private folders, BitsOS's own programs);
//! - `store`: one merged line per day and thing under /data/system/access, retention by days and size, the
//!   first-time marks ("Neu");
//! - `query`: the socket's and the MCP tool's answers (`summary`, `events`);
//! - `settings`: the `access.*` keys; `serve`: the service.
#![forbid(unsafe_code)]

pub mod kernel;
pub mod model;
pub mod query;
pub mod serve;
pub mod settings;
pub mod store;

pub use bitsjournal::now_ms;

pub const DIR: &str = "/data/system/access";
pub const NODE: &str = "/dev/bitsos/access";
pub const SOCKET: &str = "/run/bitsos/access.sock";
/// The session's user: besides root the only one the socket answers (Georg's desktop and its programs).
pub const SESSION_UID: u32 = 1000;
