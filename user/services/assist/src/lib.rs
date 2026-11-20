//! assist (CL-3a, V2-PLAN §8a.3): the native assistant client of BitsOS. A console chat with the AI provider
//! Georg chose (DECISIONS 21.10.: none is built in by name), one adapter per provider kind behind
//! [`provider::Adapter`], tool use through the settings registry with a permission level per tool.
//!
//! The library has no terminal in it below [`chat`] and [`term`]: CL-3b puts a window on [`engine::Session`]
//! through the [`engine::Ui`] trait without touching the adapters.
#![deny(unsafe_code)]

pub mod chat;
pub mod cli;
pub mod config;
pub mod conv;
pub mod de;
pub mod engine;
pub mod limits;
pub mod mcp;
pub mod net;
pub mod once;
pub mod probe;
pub mod prompt;
pub mod provider;
pub mod secret;
pub mod serve;
pub mod settings;
pub mod term;
pub mod tools;

pub use settingsd::json;
