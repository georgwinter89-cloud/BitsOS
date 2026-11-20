//! bitsbench: the nightly benchmark runner (SELFOPT-1).
//!   bitsbench serve [--dir D]                 the nightly keeper (bitscomp starts it, uid 1000)
//!   bitsbench run [--dir D] [--only a,b] [--runs N] [--no-mute]
//!                                             the whole set now, rules aside (a first run with Georg watching,
//!                                             the CachyOS comparison); input still stops it
//!   bitsbench status [--dir D]                the settings, the facts and whether a night run would start now
//!   bitsbench report [--dir D]                the last morning line
//!   bitsbench enter ID VALUE [--night DATE] [--dir D]
//!                                             a value measured by hand (farthest_frontier, ms per frame)
//!   bitsbench import-cachyos FILE [--dir D]   the CachyOS run's results.json for the page's comparison row
//!   bitsbench ton                             each sound card's master switch (the checks)
//!   bitsbench fake brave|game|bitsprof ...    stand-ins for QEMU and the host tests
//! Exit: 0 done, 1 refused or failed, 2 usage, 3 stopped by input.

use bitsbench::comp::Comp;
use bitsbench::config::Config;
use bitsbench::night::{End, Run};
use bitsbench::store::{self, Night, Outcome};
use std::path::{Path, PathBuf};

fn usage() -> i32 {
    eprintln!("usage: bitsbench serve|run|status|report|enter|import-cachyos|ton|fake ... (see the head of main.rs)");
    2
}

fn flag(args: &mut Vec<String>, name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.remove(i);
    (i < args.len()).then(|| args.remove(i))
}

fn switch(args: &mut Vec<String>, name: &str) -> bool {
    let had = args.iter().any(|a| a == name);
    args.retain(|a| a != name);
    had
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        std::process::exit(usage());
    }
    let cmd = args.remove(0);
    if cmd == "fake" {
        let code = match args.first().map(String::as_str) {
            Some("brave") => bitsbench::fake::brave(),
            Some("game") => bitsbench::fake::game(),
            Some("bitsprof") => bitsbench::fake::bitsprof(&args[1..]),
            _ => usage(),
        };
        std::process::exit(code);
    }
    let root = PathBuf::from(flag(&mut args, "--dir").unwrap_or_else(|| bitsbench::DIR.into()));
    let code = match cmd.as_str() {
        "serve" => bitsbench::serve::serve(&root),
        "run" => run(&root, &mut args),
        "status" => status(&root),
        "report" => report(&root),
        "enter" => enter(&root, &mut args),
        "import-cachyos" => import(&root, &args),
        "ton" => {
            print!("{}", bitsbench::ton::describe(&bitsbench::ton::System));
            0
        }
        _ => usage(),
    };
    std::process::exit(code);
}

fn run(root: &Path, args: &mut Vec<String>) -> i32 {
    let bitsos = bitsbench::probe::is_bitsos();
    let only = flag(args, "--only");
    let runs = flag(args, "--runs");
    let no_mute = switch(args, "--no-mute");
    let mut cfg = Config::load(root, bitsos);
    if let Some(o) = only {
        cfg.only = o.split(',').filter(|s| !s.is_empty()).map(String::from).collect();
    }
    if let Some(r) = runs {
        match r.parse::<usize>() {
            Ok(n) if (1..=bitsbench::catalog::RUNS).contains(&n) => cfg.runs = n,
            _ => return usage(),
        }
    }
    let _claim = match bitsbench::serve::claim(&root.join("run.pid")) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("bitsbench: {e}");
            return 1;
        }
    };
    let comp = Comp::from_env();
    let reach = comp.idle().is_ok();
    // On BitsOS the sound must go off; --no-mute is only for another Linux (Georg turns his sound off himself).
    let end = Run { cfg: &cfg, comp: reach.then_some(&comp), mixer: &bitsbench::ton::System, need_mute: bitsos || !no_mute }.go();
    match end {
        End::Done { night, line } => {
            println!("{line}");
            println!("{}", store::bench_dir(root).join(night).join("results.json").display());
            0
        }
        End::Stopped => {
            println!("Abgebrochen: jemand hat den Rechner benutzt. Die Messung dieser Nacht ist verworfen.");
            3
        }
        End::Refused(why) => {
            println!("Nicht gemessen: {why}");
            1
        }
    }
}

fn status(root: &Path) -> i32 {
    let (s, note) = bitsbench::settings::load(&bitsbench::settings::socket());
    let comp = Comp::from_env();
    let desk = comp.idle();
    let facts = bitsbench::rules::Facts {
        now_s: bitsbench::time::now_s(),
        desk: desk.as_ref().ok().copied(),
        power: bitsbench::probe::power(),
        rx_bps: Some(0.0),
        last_done_s: store::read_json(&store::bench_dir(root).join("report.json")).ok().flatten().and_then(|j| j.get("done_ms").and_then(mcpreg::Json::as_u64)).map(|ms| (ms / 1000) as i64),
        sound: bitsbench::probe::sound_playing(),
    };
    println!("settings on={} from={} to={}{}", s.on, s.from_h, s.to_h, note.map(|n| format!(" note={n:?}")).unwrap_or_default());
    match &desk {
        Ok(d) => println!("desk idle_s={} inputs={} fullscreen={} games={} offset_min={}", d.idle_s, d.inputs, d.fullscreen, d.games, d.offset_min),
        Err(e) => println!("desk unreachable: {e}"),
    }
    println!("power={:?} sound={} generation={:?} origin={:?}", facts.power, facts.sound, bitsbench::probe::generation(), bitsbench::probe::origin());
    match bitsbench::rules::decide(&s, &facts) {
        Ok(()) => println!("verdict run (download not measured here)"),
        Err(w) => println!("verdict wait {}", w.word()),
    }
    0
}

fn report(root: &Path) -> i32 {
    match store::read_json(&store::bench_dir(root).join("report.json")) {
        Ok(Some(j)) => {
            println!("{}", j.get("line").and_then(mcpreg::Json::as_str).unwrap_or(""));
            0
        }
        _ => {
            println!("Noch keine Nacht-Messung.");
            1
        }
    }
}

fn newest_night(root: &Path) -> Option<String> {
    let mut v: Vec<String> = std::fs::read_dir(store::bench_dir(root)).ok()?.flatten().filter_map(|e| e.file_name().into_string().ok()).filter(|n| store::is_night_name(n)).collect();
    v.sort();
    v.pop()
}

fn enter(root: &Path, args: &mut Vec<String>) -> i32 {
    let night = flag(args, "--night").or_else(|| newest_night(root));
    let (Some(id), Some(value), Some(night)) = (args.first(), args.get(1), night) else { return usage() };
    if !bitsbench::catalog::get(id).is_some_and(|b| b.by_hand) {
        eprintln!("bitsbench: {id} is not entered by hand (those are: farthest_frontier)");
        return 2;
    }
    let Some(v) = value.replace(',', ".").parse::<f64>().ok().filter(|v| v.is_finite() && *v > 0.0) else { return usage() };
    let p = store::bench_dir(root).join(&night).join("results.json");
    let Some(mut n) = store::read_json(&p).ok().flatten().as_ref().and_then(Night::from_json) else {
        eprintln!("bitsbench: no results in {}", p.display());
        return 1;
    };
    n.benches.insert(id.clone(), Outcome::Runs(vec![v]));
    let h = store::history_with(store::load_history(root), store::Entry::of(&n));
    let r = store::write_json(&p, &n.to_json()).and_then(|_| store::write_json(&store::bench_dir(root).join("history.json"), &store::history_to_json(&h)));
    match r {
        Ok(()) => {
            bitsbench::events::event("bench.entered", &format!("id={id} night={night} value={v}"));
            0
        }
        Err(e) => {
            eprintln!("bitsbench: {e}");
            1
        }
    }
}

fn import(root: &Path, args: &[String]) -> i32 {
    let Some(file) = args.first() else { return usage() };
    let n = match store::read_json(Path::new(file)) {
        Ok(Some(j)) => Night::from_json(&j),
        Ok(None) => None,
        Err(e) => {
            eprintln!("bitsbench: {e}");
            return 1;
        }
    };
    let Some(n) = n.filter(|n| n.system == "linux") else {
        eprintln!("bitsbench: {file} is not the results of a run under another Linux");
        return 1;
    };
    match store::write_json(&root.join("cachyos.json"), &n.to_json()) {
        Ok(()) => {
            println!("Vergleich übernommen: {} Messungen vom {}", n.medians().len(), bitsbench::time::short_date(&n.night));
            0
        }
        Err(e) => {
            eprintln!("bitsbench: {e}");
            1
        }
    }
}
