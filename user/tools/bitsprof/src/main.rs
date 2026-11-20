//! bitsprof (SELFOPT-1): the sampling profiler of the self-optimisation, level 1 (plan/spec/self-optimise.md).
//!
//! ```text
//! bitsprof run --out DIR [--hz 1000] [--seconds N] [--max-mb 64] [--pages 64]
//! bitsprof report DIR [--top N] [--json]
//! ```
//! `run` samples every CPU about `hz` times a second (the kernel's cpu-clock perf event) until SIGTERM, SIGINT or
//! `--seconds`, and keeps DIR/profile.bin (functions already named, capped in size: the oldest stretches go
//! first) and DIR/meta.json. `report` lists the hottest functions, the kernel's and each program's. Runs
//! unchanged on BitsOS and on Linux (as root, or with `kernel.perf_event_paranoid` at -1).
#![forbid(unsafe_code)]

mod elf;
mod model;
mod report;
mod run;
mod store;
mod syms;
mod v0;

use std::path::PathBuf;

/// Marks an error of the work itself (no usage text after it).
const FAILED: &str = "\u{0}";
const USAGE: &str = "Aufruf:\n  bitsprof run --out ORDNER [--hz 1000] [--seconds N] [--max-mb 64] [--pages 64]\n  bitsprof report ORDNER [--top N] [--json]\n";

fn num(v: Option<String>, what: &str) -> Result<u64, String> {
    v.ok_or(format!("{what}: Zahl fehlt"))?.parse().map_err(|_| format!("{what}: keine Zahl"))
}

fn main() {
    let mut args = std::env::args().skip(1);
    let r = match args.next().as_deref() {
        Some("run") => {
            let mut o = run::Opts { out: PathBuf::new(), hz: 1000, seconds: None, max_bytes: 64 << 20, pages: 64 };
            let mut err = None;
            while let Some(a) = args.next() {
                let r = match a.as_str() {
                    "--out" => args.next().map(|v| o.out = PathBuf::from(v)).ok_or_else(|| "--out: Ordner fehlt".to_string()),
                    "--hz" => num(args.next(), "--hz").map(|v| o.hz = v.clamp(1, 10_000)),
                    "--seconds" => num(args.next(), "--seconds").map(|v| o.seconds = Some(v)),
                    "--max-mb" => num(args.next(), "--max-mb").map(|v| o.max_bytes = (v.max(1) as usize) << 20),
                    "--pages" => num(args.next(), "--pages").map(|v| o.pages = (v.clamp(1, 1024) as usize).next_power_of_two()),
                    _ => Err(format!("unbekannte Angabe {a}")),
                };
                if let Err(e) = r {
                    err = Some(e);
                    break;
                }
            }
            match err {
                Some(e) => Err(e),
                None if o.out.as_os_str().is_empty() => Err("--out fehlt".into()),
                None => run::run(&o).map(|_| String::new()).map_err(|e| format!("{FAILED}{e}")),
            }
        }
        Some("report") => {
            let (mut dir, mut n, mut json, mut err) = (None, 10usize, false, None);
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--top" => match num(args.next(), "--top") {
                        Ok(v) => n = v.max(1) as usize,
                        Err(e) => err = Some(e),
                    },
                    "--json" => json = true,
                    d if dir.is_none() && !d.starts_with("--") => dir = Some(PathBuf::from(d)),
                    _ => err = Some(format!("unbekannte Angabe {a}")),
                }
            }
            match (err, dir) {
                (Some(e), _) => Err(e),
                (None, None) => Err("Ordner fehlt".into()),
                (None, Some(d)) => report::report(&d, n, json).map_err(|e| format!("{FAILED}{e}")),
            }
        }
        _ => Err(String::new()),
    };
    match r {
        Ok(out) => print!("{out}"),
        // A failed measurement or report: its reason only.
        Err(e) if e.starts_with(FAILED) => {
            eprintln!("bitsprof: {}", &e[FAILED.len()..]);
            std::process::exit(1);
        }
        Err(e) => {
            if !e.is_empty() {
                eprintln!("bitsprof: {e}");
            }
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    }
}
