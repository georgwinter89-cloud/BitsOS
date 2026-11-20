//! bitsbench (SELFOPT-1, level 1 of plan/spec/self-optimise.md): the nightly benchmark set. On by default (DECISIONS
//! 18.11.; Georg switches it off on the settings page "Leistung"), it runs once a night inside its window, on mains power, when
//! nobody used the machine for 30 minutes and no game, full-screen program or download runs. Each benchmark runs
//! 5 times; median and spread go to `/data/leistung/bench/<date>/results.json`, the profiler `bitsprof` samples
//! beside it, and one plain-German line is the morning's report. Sound is muted for the whole run and restored
//! after; any input ends the run at once and throws the night's partial results away.
#![forbid(unsafe_code)]

pub mod bench;
pub mod catalog;
pub mod cdp;
pub mod comp;
pub mod config;
pub mod events;
pub mod fake;
pub mod group;
pub mod night;
pub mod probe;
pub mod prof;
pub mod report;
pub mod rules;
pub mod serve;
pub mod settings;
pub mod stats;
pub mod store;
pub mod time;
pub mod ton;

/// Where everything of the benchmark lives (Georg's data partition on the Mini-PC, the test disk under QEMU).
pub const DIR: &str = "/data/leistung";
