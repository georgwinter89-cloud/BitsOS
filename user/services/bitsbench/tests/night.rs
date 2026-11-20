//! A whole night with the stand-ins (the brave, game and profiler fakes of the binary, a fake desktop on a
//! control socket, fake sound cards): the files it leaves, the sound around it, and a run stopped by input.

use bitsbench::comp::Comp;
use bitsbench::config::Config;
use bitsbench::night::{End, Run};
use bitsbench::ton::emu::Cards;
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_bitsbench");

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("bitsbench-night-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A desktop that answers `idle` (its input count moves once `idle` was asked `input_after` times) and `stats`.
fn desk(dir: &std::path::Path, input_after: u64) -> (Comp, Arc<AtomicU64>) {
    let path = dir.join("wayland-test.ctl");
    let l = UnixListener::bind(&path).unwrap();
    let asked = Arc::new(AtomicU64::new(0));
    let a = asked.clone();
    std::thread::spawn(move || {
        let mut frames = 0u64;
        for s in l.incoming().flatten() {
            let mut line = String::new();
            let mut s2 = s.try_clone().unwrap();
            if BufReader::new(s).read_line(&mut line).is_err() {
                continue;
            }
            let ans = match line.trim() {
                "idle" => {
                    let n = a.fetch_add(1, Ordering::SeqCst) + 1;
                    let inputs = if n > input_after { 8 } else { 7 };
                    format!("idle input_s=4000 inputs={inputs} fullscreen=0 games=0 offset_min=60\n")
                }
                "stats" => {
                    frames += 25;
                    format!("frames={frames} render_avg_ms=2.000 output=1280x800\n")
                }
                other => format!("unknown command '{other}'\n"),
            };
            let _ = s2.write_all(ans.as_bytes());
        }
    });
    (Comp { path }, asked)
}

fn config(root: &std::path::Path) -> Config {
    let mut c = Config::defaults(root, true);
    let fake = |w: &str| vec![BIN.to_string(), "fake".into(), w.into()];
    c.brave = fake("brave");
    c.game = fake("game");
    c.bitsprof = fake("bitsprof");
    c.only = ["brave_start", "game_frame", "file_read", "file_mix"].iter().map(|s| s.to_string()).collect();
    c.runs = 2;
    c
}

fn cards() -> Cards {
    Cards(Rc::new(RefCell::new(vec![Some([1, 1])])), Rc::new(RefCell::new(false)))
}

#[test]
fn a_night_with_the_stand_ins_leaves_its_results_and_the_sound_as_it_was() {
    let root = tmp("done");
    let cfg = config(&root);
    let (comp, asked) = desk(&root, u64::MAX);
    let c = cards();
    let end = Run { cfg: &cfg, comp: Some(&comp), mixer: &c, need_mute: true }.go();
    let End::Done { night, line } = end else { panic!("{end:?}") };
    assert_eq!(line, "Nacht-Messung: erste Messung, noch nichts zum Vergleichen");
    assert!(asked.load(Ordering::SeqCst) > 2, "input was looked for during the run");
    let b = root.join("bench");
    let r = bitsbench::store::read_json(&b.join(&night).join("results.json")).unwrap().unwrap();
    let n = bitsbench::store::Night::from_json(&r).unwrap();
    let med = n.medians();
    for id in ["brave_start", "brave_load", "brave_scroll", "game_frame", "file_read", "file_mix"] {
        assert!(med.get(id).is_some_and(|v| *v > 0.0), "{id} in {r}");
        assert!(matches!(&n.benches[id], bitsbench::store::Outcome::Runs(v) if v.len() == 2), "{id}: two runs");
    }
    assert!((med["game_frame"] - 4200.0 / 969.0).abs() < 0.01);
    assert_eq!(n.profile, Ok("profil/report.json".to_string()));
    assert!(b.join(&night).join("profil/meta.json").is_file() && b.join(&night).join("profil/report.json").is_file());
    let meta = std::fs::read_to_string(b.join(&night).join("profil/meta.json")).unwrap();
    assert!(meta.contains(&format!(r#""seconds":{}"#, bitsbench::prof::LIMIT_S)), "the profiler ends by itself if nobody stops it: {meta}");
    assert!(!root.join("run.children").exists(), "every program of the run ended, the list is gone");
    let h = bitsbench::store::load_history(&root);
    assert_eq!(h.len(), 1);
    let rep = bitsbench::store::read_json(&b.join("report.json")).unwrap().unwrap();
    assert_eq!(rep.get("line").and_then(mcpreg::Json::as_str), Some(line.as_str()));
    assert_eq!(*c.0.borrow(), vec![Some([1, 1])], "sound back on");
    assert!(!root.join("ton.json").exists());
    assert!(!root.join("tmp").exists(), "the test folder is removed after");
    assert!(root.join("seite/index.html").is_file());
}

#[test]
fn input_stops_the_run_and_the_night_is_thrown_away() {
    let root = tmp("stop");
    let cfg = config(&root);
    let (comp, _) = desk(&root, 3);
    let c = cards();
    // A night measured earlier on the same date stays as it was.
    let date = bitsbench::time::Local::of(bitsbench::time::now_s(), 60).date();
    let earlier = root.join("bench").join(&date);
    std::fs::create_dir_all(&earlier).unwrap();
    std::fs::write(earlier.join("results.json"), "{}").unwrap();
    let end = Run { cfg: &cfg, comp: Some(&comp), mixer: &c, need_mute: true }.go();
    assert_eq!(end, End::Stopped);
    let nights: Vec<_> = std::fs::read_dir(root.join("bench")).map(|r| r.flatten().map(|e| e.file_name().into_string().unwrap()).collect()).unwrap_or_default();
    assert_eq!(nights, vec![date], "nothing of the stopped night is kept");
    assert_eq!(std::fs::read_to_string(earlier.join("results.json")).unwrap(), "{}");
    assert_eq!(*c.0.borrow(), vec![Some([1, 1])], "sound back on after the stop");
    assert!(!root.join("tmp").exists());
    assert!(!root.join("bench/report.json").exists(), "no morning line for a thrown-away night");
}

#[test]
fn without_a_card_to_mute_nothing_runs() {
    let root = tmp("nomute");
    let cfg = config(&root);
    let none = Cards(Rc::new(RefCell::new(vec![])), Rc::new(RefCell::new(false)));
    let end = Run { cfg: &cfg, comp: None, mixer: &none, need_mute: true }.go();
    assert!(matches!(end, End::Refused(ref w) if w.starts_with("Ton nicht stumm")), "{end:?}");
    assert!(std::fs::read_dir(root.join("bench")).map(|r| r.count()).unwrap_or(0) == 0);
}

#[test]
fn a_missing_program_is_said_not_failed() {
    let root = tmp("missing");
    let mut cfg = config(&root);
    cfg.brave = vec!["/nonexistent/brave".into()];
    cfg.only = vec!["brave_start".into()];
    let c = cards();
    let End::Done { night, .. } = Run { cfg: &cfg, comp: None, mixer: &c, need_mute: true }.go() else { panic!() };
    let r = bitsbench::store::read_json(&root.join("bench").join(night).join("results.json")).unwrap().unwrap().to_string();
    assert!(r.contains(r#""brave_scroll":{"missing":"Brave ist auf diesem Rechner nicht da""#), "{r}");
}

#[test]
fn a_desktop_that_does_not_answer_at_the_start_means_no_run() {
    let root = tmp("deaf");
    let cfg = config(&root);
    let c = cards();
    // A desktop is expected (the nightly path) but its socket is gone: no count of input to watch.
    let deaf = Comp { path: root.join("gone.ctl") };
    let end = Run { cfg: &cfg, comp: Some(&deaf), mixer: &c, need_mute: true }.go();
    assert_eq!(end, End::Refused("die Arbeitsfläche antwortet nicht".into()));
    assert_eq!(*c.0.borrow(), vec![Some([1, 1])], "nothing muted");
    assert!(!root.join("ton.json").exists());
    assert!(!root.join("bench").exists(), "nothing started, nothing written");
}

#[test]
fn what_a_killed_run_left_is_gone_at_the_next_start() {
    let root = tmp("left");
    let cfg = config(&root);
    let (comp, _) = desk(&root, u64::MAX);
    let c = cards();
    let b = root.join("bench");
    // An unfinished night of an earlier date with its profile, a finished one, the test folder.
    std::fs::create_dir_all(b.join("2026-11-01.neu/profil")).unwrap();
    std::fs::write(b.join("2026-11-01.neu/profil/profile.bin"), "x").unwrap();
    std::fs::create_dir_all(b.join("2026-11-02")).unwrap();
    std::fs::create_dir_all(root.join("tmp/brave")).unwrap();
    // A profiler the killed run started, noted in run.children and never stopped.
    let kids = bitsbench::group::Kids::at(&root);
    let mut sleeper = std::process::Command::new("/bin/sh");
    sleeper.args(["-c", "sleep 120"]);
    let g = bitsbench::group::Group::spawn(&mut sleeper, &kids).unwrap();
    let pid = g.id();
    std::mem::forget(g);
    assert!(root.join("run.children").is_file());
    let mut cfg2 = cfg.clone();
    cfg2.only = vec!["file_read".into()];
    let End::Done { .. } = Run { cfg: &cfg2, comp: Some(&comp), mixer: &c, need_mute: true }.go() else { panic!() };
    assert!(!b.join("2026-11-01.neu").exists(), "an unfinished night of any date goes");
    assert!(b.join("2026-11-02").is_dir(), "a finished night stays");
    assert!(!root.join("tmp").exists());
    assert!(!root.join("run.children").exists());
    let state = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok().and_then(|t| t.rsplit_once(')').map(|(_, r)| r.trim().chars().next().unwrap_or('?')));
    assert!(matches!(state, None | Some('Z')), "the orphaned program was ended: {state:?}");
}

#[test]
fn a_dropped_program_group_ends() {
    let root = tmp("drop");
    let kids = bitsbench::group::Kids::at(&root);
    let mut c = std::process::Command::new("/bin/sh");
    c.args(["-c", "sleep 120 & sleep 120"]);
    let g = bitsbench::group::Group::spawn(&mut c, &kids).unwrap();
    let pid = g.id();
    std::thread::sleep(std::time::Duration::from_millis(200));
    drop(g);
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists(), "the leader is gone and reaped");
    assert!(usys::kill(-(pid as isize), 0).is_err(), "nothing of its group is left");
}

#[test]
fn only_groups_of_this_boot_with_their_own_leader_are_signalled() {
    use bitsbench::group::{ours, parse, stat_start};
    assert_eq!(parse("123 4567 1700000000\nbad line\n1 5 5\n"), vec![(123, 4567, 1_700_000_000)], "never pid 1");
    assert!(ours((123, 4567, 1000), Some(1010), Some(4567)));
    assert!(ours((123, 4567, 1000), Some(1010), None), "the leader gone, the group kept its number");
    assert!(!ours((123, 4567, 1000), Some(1010), Some(9999)), "the number went to another process");
    assert!(!ours((123, 4567, 1000), Some(90_000), None), "noted in an earlier boot");
    assert!(!ours((123, 4567, 1000), None, None));
    let stat = "42 (a (b) c) S 1 42 42 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 98765 1000 10";
    assert_eq!(stat_start(stat), Some(98765));
}

#[test]
fn fills_of_gone_runners_are_removed() {
    let d = tmp("fills");
    let me = std::process::id();
    for n in ["bitsbench-last-999999999".to_string(), format!("bitsbench-last-{me}"), "other".into()] {
        std::fs::write(d.join(n), "x").unwrap();
    }
    assert_eq!(bitsbench::bench::memory::tidy_fills(&d), 1);
    assert!(d.join(format!("bitsbench-last-{me}")).exists() && d.join("other").exists());
}
