//! SYS-A: every tab built and drawn from a machine like the Mini-PC and from one without any source: real values
//! where there are some, "–" with the reason where not, nothing invented; masked values for reference pictures;
//! the task manager's detail with its rules.

use super::*;
use crate::layout::Rect;
use crate::sys_group::{Part, Row, State, Win};
use crate::sys_parse::Mem;
use crate::sys_rules::{Gate, Vorrang};
use crate::sys_snap::{Cpu, Disk, Facts, Gpu, Heat, Kernel, Memory, Mount, Net};
use crate::sys_view::{Tab, View};

pub fn snapshot() -> Snapshot {
    Snapshot {
        t: 100,
        wall: 1_794_591_120,
        hz: 100,
        cpu: Cpu { busy: Some(6.0), programs: Some(4.0), kernel: Some(2.0), threads: vec![Some(12.0), Some(4.0), Some(31.0), None], cur_khz: Some(3_800_000), max_khz: Some(5_100_000), switches: Some(8400.0), load: Some((0.74, 1.29, 1.56)), running: Some(2), all_threads: Some(419) },
        mem: Memory { mem: Some(Mem { total: 25_560_000_000, free: 19_000_000_000, available: 21_000_000_000, buffers: 100_000_000, cached: 2_000_000_000, shmem: 300_000_000, reclaimable: 200_000_000, swap_total: 0, swap_free: 0 }) },
        gpu: Gpu { busy: Some(31.0), vram: Some((1_500_000_000, 8 << 30)), gtt: Some((600_000_000, 12 << 30)), temp: Some(48.0), power: Some(9.0), sclk_mhz: Some(1900.0), via: "sysfs", ..Gpu::default() },
        disks: vec![Disk { name: "nvme0n1".into(), size: Some(1_000_204_886_016), model: Some("Lexar SSD NM790 1TB".into()), rotational: Some(false), read: Some(12.0 * 1_048_576.0), write: Some(400_000.0), read_total: Some(12 << 40), write_total: Some(8 << 40), parts: vec![("nvme0n1p1".into(), 470 << 30), ("nvme0n1p3".into(), 510 << 30)] }],
        mounts: vec![Mount { dev: "/dev/nvme0n1p3".into(), path: "/data".into(), used: 197 << 30, total: 980 << 30 }, Mount { dev: "/dev/sda1".into(), path: "/boot".into(), used: 4 << 30, total: 16 << 30 }],
        nets: vec![Net { name: "igc0".into(), rx: Some(2.4 * 1_048_576.0), tx: Some(100_000.0), rx_total: 53_900_000_000, tx_total: 1_200_000_000, up: Some(true), speed_mbps: Some(2500), wifi: None }],
        heat: Heat { cpu: Some(52.0), gpu: Some(48.0), nvme: Some(41.0), cpu_power: Some(18.0), gpu_power: Some(9.0), all: vec![("k10temp".into(), "Tctl".into(), 52.0)] },
        kernel: Kernel { generation: Some((151, "0.1.0".into())), boot_ms: Some(9800), uptime: Some(6840.0), boot_wall: Some(1_794_584_280), crash: Some((1_794_591_120, 150)), crashes: Some(0.0), calls: Some(21_400.0), refused: Some(0.3), oom: Some(0.0), reclaim: Some(0.0) },
        facts: Facts { cpu_model: Some("AMD Ryzen 7 PRO 8845HS".into()), cores: Some(8), threads: Some(16), gpu_name: Some("Radeon 780M".into()), mem_total: Some(25_560_000_000), dmi: vec![("Modell".into(), "UM890 Pro".into())], usb: vec![("Logitech USB Receiver".into(), "USB 1".into())], pci: Some(47), inputs: Vec::new() },
        procs_read: true,
        proc_count: Some(40),
        ..Snapshot::default()
    }
}

pub fn rows() -> Vec<Row> {
    let r = |name: &str, part: Part, pid: u32, uid: u32, cpu: f64, service: bool| Row {
        part: Some(part),
        name: name.into(),
        pids: vec![pid],
        uid: Some(uid),
        cpu: Some(cpu),
        mem: Some(100 << 20),
        state: Some(State::Runs),
        service,
        start: 1000 + pid as u64,
        comm: name.to_lowercase(),
        threads: 4,
        ..Row::default()
    };
    let mut game = r("Farthest Frontier", Part::Window, 2817, 1000, 4.1, false);
    game.wins = vec![Win { id: 15, pid: 2817, app_id: "steam_app_1044720".into(), title: "Farthest Frontier".into(), hung: false, x11: true }];
    game.app = "steam_app_1044720".into();
    game.argv = vec!["/x/common/Proton 9.0/proton".into(), "run".into()];
    game.note = "über Steam".into();
    let mut desk = r("Desktop", Part::System, 410, 1000, 0.6, true);
    desk.core = true;
    vec![game, r("claude", Part::Background, 3300, 1000, 0.0, false), desk, r("Protokoll", Part::System, 440, 0, 0.0, true)]
}

fn ctx<'a>(s: &'a Snapshot, h: &'a History, v: &'a View, rows: &'a [Row], d: &'a Desk) -> Ctx<'a> {
    Ctx { s, h, v, rows, desk: d, off_min: 60, journal: None, own_uid: 1000, real_root: true }
}

fn desk() -> Desk {
    Desk { fps: Some(60.0), build_ms: Some(0.5), render_ms: Some(5.6), present_ms: Some(12.7), input_ms: Some(12.1), frames: 353_450, size: Some((2560, 1440)), refresh_hz: Some(60.0), hung: 0 }
}

fn text(tab: Tab, s: &Snapshot, mask: bool) -> String {
    let h = History::default();
    let v = View { tab, mask, ..View::default() };
    let rows = rows();
    let d = desk();
    crate::sys_report::tab_text(&build(&ctx(s, &h, &v, &rows, &d)))
}

#[test]
fn the_overview_shows_the_machine_as_the_design_does() {
    let t = text(Tab::Overview, &snapshot(), false);
    for want in [
        "status | Alles in Ordnung",
        "sub | Läuft seit 1 Std 54 Min · Generation 151",
        "big | 6 % | 16 Threads",
        "sub | Last 0,74 · 1,29 · 1,56",
        "big | 31 % | 2560 × 1440",
        "sub | Bild zeichnen 5,6 ms · zeigen 12,7 ms",
        "Daten (/data) | 197,0 GB / 980,0 GB",
        "Prozessor-Paket | 18 W",
        "Farthest Frontier | 4,1 % · 100 MB",
        "Prozessor | AMD Ryzen 7 PRO 8845HS",
        "Abstürze | keiner seit dem Start",
        "link | Alle 40 Prozesse | tab=prozesse",
    ] {
        assert!(t.contains(want), "{want:?} missing in\n{t}");
    }
}

#[test]
fn a_machine_without_sources_shows_dashes_and_why() {
    let empty = Snapshot::default();
    for tab in Tab::ALL {
        let t = text(tab, &empty, false);
        assert!(!t.contains("NaN") && !t.contains("inf"), "{tab:?}: {t}");
    }
    let g = text(Tab::Gpu, &empty, false);
    assert!(g.contains("big | – |") && g.contains(crate::sys_text::NO_SOURCE), "{g}");
    let p = text(Tab::Power, &empty, false);
    assert!(p.contains("Prozessor | –") && p.contains(crate::sys_text::NO_SOURCE), "{p}");
    let o = text(Tab::Overview, &empty, false);
    assert!(o.contains("big | – |") && o.contains("Abstürze | keiner gespeichert"), "{o}");
    let c = text(Tab::Cpu, &empty, false);
    assert!(c.contains("mid | –") && c.contains(crate::sys_text::NO_SOURCE), "{c}");
}

#[test]
fn masked_values_do_not_depend_on_the_machine() {
    let a = text(Tab::Overview, &snapshot(), true);
    let b = text(Tab::Overview, &Snapshot { cpu: Cpu { busy: Some(77.0), ..snapshot().cpu }, ..snapshot() }, true);
    assert_eq!(a, b);
    assert!(a.contains(MASK) && !a.contains("6 %"), "{a}");
    let p = text(Tab::Procs, &snapshot(), true);
    assert!(p.contains("[Mit Fenster]") && p.contains("[BitsOS selbst]") && p.contains("counts | 0 0 0 0"), "{p}");
}

#[test]
fn the_task_manager_detail_follows_the_rules() {
    let s = snapshot();
    let h = History::default();
    let rows = rows();
    let d = desk();
    let pick = |pid: u32| {
        let v = View { tab: Tab::Procs, selected: Some((pid, 1000 + pid as u64)), ..View::default() };
        build(&ctx(&s, &h, &v, &rows, &d)).procs.unwrap().detail.unwrap()
    };
    let game = pick(2817);
    assert_eq!(game.end, Gate::Free);
    assert_eq!(game.prio, [Gate::Free, Gate::Free, Gate::Ask]);
    assert!(game.show);
    assert!(game.rows.contains(&("Läuft mit".into(), "Proton 9.0 · Windows-Spiel".into())), "{:?}", game.rows);
    assert!(game.line.contains("steam_app_1044720 · Prozess 2817 · seit") && game.line.contains("Fenster w15"), "{}", game.line);
    assert_eq!(game.vorrang, Vorrang::Normal);
    let desk = pick(410);
    assert_eq!(desk.end, Gate::No(crate::sys_rules::CORE.into()));
    assert_eq!(desk.prio, [Gate::Ask, Gate::Ask, Gate::Ask]);
    let journal = pick(440);
    assert!(matches!(journal.end, Gate::No(_)) && journal.prio.iter().all(|g| matches!(g, Gate::No(_))), "root's service: nothing");
    // Nothing chosen yet: the first row is shown (the page pins it as its choice when it draws it), its buttons wait.
    let v = View { tab: Tab::Procs, ..View::default() };
    let first = build(&ctx(&s, &h, &v, &rows, &d)).procs.unwrap().detail.unwrap();
    assert_eq!((first.pid, first.show), (2817, false));
    assert_eq!(first.end, Gate::No(crate::sys_model3::PICK.into()));
    // A program chosen before that ended: never another one in its place, and nothing to press.
    let v = View { tab: Tab::Procs, selected: Some((9999, 1)), ..View::default() };
    let p = build(&ctx(&s, &h, &v, &rows, &d)).procs.unwrap();
    let gone = p.detail.unwrap();
    assert_eq!((gone.pid, gone.name.as_str(), gone.show), (9999, "Läuft nicht mehr", false));
    let no = Gate::No(crate::sys_model3::GONE.into());
    assert_eq!((gone.end, gone.prio), (no.clone(), [no.clone(), no.clone(), no]));
    assert!(p.sections.iter().flat_map(|s| &s.lines).all(|l| !l.selected), "no row lights up in its place");
    // The same pid started again (another start time) is another program.
    let v = View { tab: Tab::Procs, selected: Some((2817, 1)), ..View::default() };
    assert_eq!(build(&ctx(&s, &h, &v, &rows, &d)).procs.unwrap().detail.unwrap().name, "Läuft nicht mehr");
}

#[test]
fn the_buttons_act_only_on_the_explicit_choice() {
    use crate::sys_model3::{pin, selected};
    let rows = rows();
    let shown = crate::sys_group::shown(&rows, crate::sys_group::Filter::All, "", crate::sys_group::Column::Cpu, false, 3);
    assert!(selected(&rows, None).is_none(), "no choice: no target, whatever the table shows first");
    assert_eq!(selected(&rows, Some((3300, 4300))).map(|r| r.name.as_str()), Some("claude"));
    assert!(selected(&rows, Some((3300, 1))).is_none(), "a reused pid is not the program chosen");
    assert!(selected(&rows, Some((9999, 1))).is_none(), "an ended choice falls back to nothing");
    assert_eq!(pin(&rows, &shown, None), Some((2817, 3817)), "the shown first row becomes the choice");
    assert_eq!(pin(&rows, &shown, Some((9999, 1))), None, "an ended choice is not replaced");
}

#[test]
fn no_verdict_and_no_rates_before_the_first_measurement() {
    let h = History::default();
    let rows = rows();
    let d = desk();
    let v = View::default();
    let empty = Snapshot::default();
    assert_eq!(status(&ctx(&empty, &h, &v, &rows, &d)), (MEASURING.to_string(), Tone::Muted), "not \"Alles in Ordnung\" without data");
    let mut first = snapshot();
    first.cpu.busy = None;
    assert_eq!(status(&ctx(&first, &h, &v, &rows, &d)).0, MEASURING, "one sample has no processor share yet");
    assert_eq!(status(&ctx(&snapshot(), &h, &v, &rows, &d)), ("Alles in Ordnung".to_string(), Tone::Accent));
    // The first sample after opening: counters, no rates yet.
    let mut s = snapshot();
    s.nets.iter_mut().for_each(|n| (n.rx, n.tx) = (None, None));
    s.nets.push(Net { name: "wlan0".into(), ..Net::default() });
    let (rx, tx_, _, _) = net_now(&ctx(&s, &h, &v, &rows, &d));
    assert_eq!((rx, tx_), (None, None), "\"–\", not 0,0 MB/s");
    s.nets[1].rx = Some(1000.0);
    assert_eq!(net_now(&ctx(&s, &h, &v, &rows, &d)).0, Some(1000.0), "the measured ones only");
}

#[test]
fn every_tab_draws_at_any_size_and_scale() {
    let s = snapshot();
    let mut h = History::default();
    for t in 0..120u64 {
        h.add(t, "cpu", Some((t % 17) as f64));
        h.add(t, "cpu.programs", Some((t % 11) as f64));
        h.add(t, "desk.frame_ms", Some(if t == 50 { 40.0 } else { 16.0 }));
    }
    let rows = rows();
    let d = desk();
    let mut t125 = crate::theme::test_theme().clone();
    t125.scale = 1.25;
    for theme in [crate::theme::test_theme(), &t125] {
        for c in [Rect { x: 0, y: 0, w: 1440, h: 900 }, Rect { x: 40, y: 30, w: 640, h: 420 }] {
            for tab in Tab::ALL {
                let v = View { tab, ..View::default() };
                let m = build(&ctx(&s, &h, &v, &rows, &d));
                let lay = crate::sys_layout::layout(c, &m, tab, m.status.as_ref().map(|x| x.0.as_str()), 0);
                let look = crate::sys_draw::SysLook { layout: lay, model: m, theme: 0, address: crate::sys_view::address(tab), tab, range: v.range, hover: None, dhover: None, field: None, msg: Some(("x".into(), false)), dialog: None };
                let canvas = crate::sys_draw::draw(&look, theme);
                let px = crate::chrome_draw::Px::new(c, theme.scale);
                assert_eq!((canvas.w, canvas.h), px.size(), "{tab:?}");
            }
        }
    }
}

#[test]
fn every_text_has_glyphs() {
    let s = snapshot();
    for tab in Tab::ALL {
        let t = text(tab, &s, false) + &text(tab, &Snapshot::default(), true);
        for ch in t.chars().filter(|c| !c.is_ascii_control()) {
            assert!(crate::text::Font::has(ch), "{ch:?} ({tab:?}) has no glyph");
        }
    }
    for extra in ["→", "▾", "▴", "↓", "↑", "…", "×", "°", "•", "„", "“"] {
        for ch in extra.chars() {
            assert!(crate::text::Font::has(ch), "{ch:?} has no glyph");
        }
    }
}

#[test]
fn the_kept_frame_draws_the_same_picture() {
    let s = snapshot();
    let h = History::default();
    let rows = rows();
    let d = desk();
    let mut t = crate::theme::test_theme().clone();
    t.scale = 1.25;
    let c = Rect { x: 0, y: 0, w: 1200, h: 700 };
    let mut keep = None;
    for tab in [Tab::Overview, Tab::Procs, Tab::Devices, Tab::Overview] {
        let v = View { tab, ..View::default() };
        let m = build(&ctx(&s, &h, &v, &rows, &d));
        let lay = crate::sys_layout::layout(c, &m, tab, None, 0);
        let look = crate::sys_draw::SysLook { layout: lay, model: m, theme: 0, address: String::new(), tab, range: v.range, hover: None, dhover: None, field: None, msg: None, dialog: None };
        // Twice: once drawing the frame, once over the kept one.
        for _ in 0..2 {
            assert!(crate::sys_draw::draw_cached(&look, &t, &mut keep).bytes() == crate::sys_draw::draw(&look, &t).bytes(), "{tab:?}");
        }
    }
}
