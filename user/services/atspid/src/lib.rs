//! Durchblick DB5a: foreign programs (Brave, LibreOffice, Steam) into the element list. atspid runs the
//! accessibility bus (busd, Rust) with the registry part programs need, reads a program's AT-SPI tree on request
//! and answers contract C3 on /run/bitsos/atspi.sock.
#![forbid(unsafe_code)]

pub mod act;
pub mod atspi;
pub mod c3;
pub mod client;
pub mod focus;
pub mod ids;
pub mod map;
pub mod peer;
pub mod read;
pub mod registry;
pub mod serve;
pub mod settings;
