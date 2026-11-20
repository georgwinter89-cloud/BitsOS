//! SYS-A: the task manager's grouping, filters, search, sort and counts.

use super::*;

fn p(pid: u32, ppid: u32, uid: u32, comm: &str, cpu: f64, rss: u64) -> Proc {
    Proc { pid, ppid, pgrp: pid, uid: Some(uid), comm: comm.into(), state: 'S', cpu: Some(cpu), rss: Some(rss), threads: 1, start: 100 + pid as u64, ..Proc::default() }
}

fn win(id: u64, pid: u32, app: &str, title: &str, hung: bool) -> Win {
    Win { id, pid, app_id: app.into(), title: title.into(), hung, x11: false }
}

fn machine() -> (Vec<Proc>, Vec<Win>) {
    let procs = vec![
        p(1, 0, 0, "init", 0.0, 1),
        p(410, 1, 1000, "bitscomp", 0.6, 240),
        p(430, 1, 1000, "ctxd", 0.2, 180),
        p(440, 1, 0, "bitsjournal", 0.0, 16),
        p(2700, 410, 1000, "steam", 0.5, 600),
        p(2701, 2700, 1000, "steamwebhelper", 0.3, 300),
        p(2790, 2700, 1000, "reaper", 0.0, 1),
        p(2817, 2790, 1000, "Game.exe", 4.1, 2300),
        p(3100, 410, 1000, "brave", 0.0, 1100),
        p(3101, 3100, 1000, "brave", 0.0, 300),
        p(3200, 410, 1000, "foot", 0.0, 18),
        p(3201, 3200, 1000, "sh", 0.0, 2),
        // A shell MCP started: Georg's background, not BitsOS.
        p(3300, 1, 1000, "claude", 0.0, 96),
        Proc { pgrp: 3300, ..p(3301, 3300, 1000, "node", 0.1, 50) },
        // Its own group (a `setsid` job): a row of its own.
        p(3400, 3300, 1000, "sleep", 0.0, 1),
    ];
    let wins = vec![win(15, 2817, "steam_app_1044720", "Farthest Frontier", false), win(9, 2700, "steam", "Steam", false), win(11, 3100, "brave-browser", "Brave", true), win(4, 3200, "foot", "Terminal", false)];
    (procs, wins)
}

fn names(id: &str) -> Option<String> {
    match id {
        "foot" => Some("Terminal".into()),
        "steam" => Some("Steam".into()),
        "brave-browser" => Some("Brave".into()),
        _ => None,
    }
}

fn grouped() -> Vec<Row> {
    let (procs, wins) = machine();
    group(&procs, &wins, &Ctx { own_pid: 410, own_uid: 1000, name_of: &names })
}

#[test]
fn windows_take_their_helpers_services_stay_apart_the_rest_is_background() {
    let rows = grouped();
    let find = |name: &str| rows.iter().find(|r| r.name == name).unwrap_or_else(|| panic!("{name} in {rows:#?}"));
    let game = find("Farthest Frontier");
    assert_eq!((game.part, game.pids.clone(), game.note.as_str()), (Some(Part::Window), vec![2817], "über Steam"));
    assert!(game.steam);
    let steam = find("Steam");
    assert_eq!(steam.part, Some(Part::Window));
    let mut sp = steam.pids.clone();
    sp.sort();
    assert_eq!(sp, vec![2700, 2701, 2790], "the helpers under Steam, the game's own window not");
    assert_eq!(steam.note, "3 Prozesse");
    assert!((steam.cpu.unwrap() - 0.8).abs() < 1e-9, "summed");
    assert_eq!(steam.mem, Some(901));
    let brave = find("Brave");
    assert_eq!((brave.state, brave.pids.len()), (Some(State::Hung), 2), "a hung window: antwortet nicht");
    let term = find("Terminal");
    assert_eq!((term.state, term.pids.clone()), (Some(State::Waits), vec![3200, 3201]));
    let desk = find("Desktop");
    assert_eq!((desk.part, desk.service, desk.core), (Some(Part::System), true, true));
    assert_eq!(desk.state, Some(State::Runs), "0,6 % works");
    for n in ["Kontextdienst", "Protokoll", "Start und Dienste"] {
        assert_eq!(find(n).part, Some(Part::System), "{n}");
    }
    assert_eq!(find("Protokoll").uid, Some(0));
    let claude = find("claude");
    assert_eq!((claude.part, claude.pids.clone()), (Some(Part::Background), vec![3300, 3301]));
    assert_eq!(find("sleep").pids, vec![3400]);
    // Every process is in exactly one row.
    let mut all: Vec<u32> = rows.iter().flat_map(|r| r.pids.clone()).collect();
    all.sort();
    let mut want: Vec<u32> = machine().0.iter().map(|p| p.pid).collect();
    want.sort();
    assert_eq!(all, want);
}

#[test]
fn filters_search_sort_and_the_shortened_bitsos_part() {
    let rows = grouped();
    assert_eq!(counts(&rows), [10, 4, 2, 4]);
    let all = shown(&rows, Filter::All, "", Column::Cpu, false, 3);
    assert_eq!(all.iter().map(|s| s.0).collect::<Vec<_>>(), [Part::Window, Part::Background, Part::System]);
    assert_eq!(all[0].1.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["Farthest Frontier", "Steam", "Brave", "Terminal"], "CPU, then name");
    assert_eq!((all[2].1.len(), all[2].2), (3, 1), "BitsOS shows its biggest three and says how many more");
    let sys = shown(&rows, Filter::System, "", Column::Cpu, false, 3);
    assert_eq!((sys.len(), sys[0].1.len(), sys[0].2), (1, 4, 0));
    let q = shown(&rows, Filter::All, "BRA", Column::Cpu, false, 3);
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].1[0].name, "Brave");
    let by_pid = shown(&rows, Filter::All, "3300", Column::Cpu, false, 3);
    assert_eq!(by_pid[0].1[0].name, "claude");
    let mem = shown(&rows, Filter::Window, "", Column::Mem, false, 3);
    assert_eq!(mem[0].1[0].name, "Farthest Frontier");
    let name_up = shown(&rows, Filter::Window, "", Column::Name, true, 3);
    assert_eq!(name_up[0].1[0].name, "Terminal", "Z-A");
    assert!(shown(&rows, Filter::Background, "nichts", Column::Cpu, false, 3).is_empty());
}

#[test]
fn a_window_of_a_bitsos_service_is_still_a_service() {
    let (mut procs, mut wins) = machine();
    procs.push(p(3500, 410, 1000, "bitsinstall", 0.0, 40));
    wins.push(win(20, 3500, "bitsinstall", "Installation", false));
    let rows = group(&procs, &wins, &Ctx { own_pid: 410, own_uid: 1000, name_of: &names });
    let inst = rows.iter().find(|r| r.main() == 3500).expect("bitsinstall's row");
    assert_eq!((inst.part, inst.service, inst.core), (Some(Part::Window), true, false), "shown with its window, guarded as a service");
    use crate::sys_rules::{Act, Gate, Vorrang, gate, learns};
    for a in [Act::End, Act::Kill, Act::Prio(Vorrang::Niedrig), Act::Prio(Vorrang::Normal)] {
        assert_eq!(gate(inst, a, 1000, true), Gate::Ask, "{a:?}");
    }
    assert!(!learns(inst, true), "K2: never a habit for BitsOS's own services");
    let game = rows.iter().find(|r| r.main() == 2817).unwrap();
    assert!(!game.service, "Georg's own program stays free");
}
