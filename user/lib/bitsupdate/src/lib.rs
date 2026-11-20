//! UPDATE (Georg 15.11.: "per Button einen neuen Kernel von der Github Adresse aufspielen"): BitsOS versions
//! from the private workshop repository. `manifest` and `sig`: what the desk signs; `verify`: what is checked
//! before anything is written; `github` and `fetchrun`: the release list and the downloads; `source`: the
//! token and the source on the boot medium; `node`: the kernel's install node; `flow`: the steps; `text`:
//! Georg's lines; `realkeys`: the console's real-keyboard yes.
#![forbid(unsafe_code)]

extern crate alloc;

pub mod confirm;
pub mod fetchrun;
pub mod flow;
pub mod github;
pub mod manifest;
pub mod node;
pub mod parts;
/// UPD-4: the parts' rules, the very file the kernel's install node is built with.
#[path = "../../../../crates/kexec/src/install/parts_rules.rs"]
pub mod parts_rules;
pub mod partsio;
/// The tar reader and writer of the installer's release assets (one file, two builds).
#[path = "../../../../host/installer/src/tar.rs"]
pub mod tar;
pub mod realkeys;
pub mod sig;
pub mod source;
pub mod text;
pub mod tty;
pub mod verify;

#[cfg(test)]
mod confirm_tests;
#[cfg(test)]
mod flow_tests;
#[cfg(test)]
mod verify_tests;
#[cfg(test)]
mod parts_tests;
