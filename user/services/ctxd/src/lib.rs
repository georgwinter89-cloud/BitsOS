//! ctxd: the BitsOS context service ("context instead of folders"), CX-1 prototype on CachyOS.
//! Design: `user/services/ctxd/DESIGN.md` (CX-0), `plan/V2-PLAN.md` §8a.4.
#![forbid(unsafe_code)]

pub mod api;
pub mod chunk;
pub mod config;
pub mod embed;
pub mod engine;
pub mod eval;
pub mod event;
pub mod extract;
pub mod indexer;
pub mod kindex;
pub mod mcp;
pub mod rank;
pub mod roots;
pub mod snippet;
pub mod sock;
pub mod store;
pub mod textnorm;
pub mod tokenize;
pub mod util;
pub mod vindex;
pub mod watch;
pub mod workspace;
