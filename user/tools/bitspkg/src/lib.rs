//! bitspkg (PK-1, V2-PLAN section 1a B2): BitsOS packages. A program is one read-only folder
//! `/apps/<name>/<version>/` with its `manifest.toml` plus one data folder `/data/appdata/<name>/`; versions are
//! switched atomically and the previous one is kept. The format is plan/PACKAGES.md.
#![forbid(unsafe_code)]

pub mod build;
pub mod clean;
pub mod cut;
pub mod elf;
pub mod foreign;
pub mod fsx;
pub mod history;
pub mod hooks;
pub mod install;
pub mod manifest;
pub mod names;
pub mod perm;
pub mod remove;
pub mod report;
pub mod selftest;
pub mod settings;
pub mod snapshot;
pub mod space;
pub mod store;
pub mod toml;
pub mod verify;
