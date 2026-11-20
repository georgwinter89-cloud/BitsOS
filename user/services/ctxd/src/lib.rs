//! ctxd: the BitsOS context service ("context instead of folders"): CX-1 prototype on CachyOS, CX-5 on V2.
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
pub mod habits;
pub mod habits_ops;
pub mod habits_rules;
pub mod habits_settings;
pub mod habits_vorrang;
pub mod kindex;
pub mod knobs;
pub mod learn;
pub mod mcp;
pub mod peer;
pub mod prio;
pub mod rank;
pub mod replay;
pub mod roots;
pub mod settings;
pub mod snippet;
pub mod sock;
pub mod store;
pub mod system;
pub mod textnorm;
pub mod tokenize;
pub mod util;
pub mod vindex;
pub mod watch;
pub mod workspace;
