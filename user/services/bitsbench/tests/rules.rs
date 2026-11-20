//! The night rules (Georg 13.11.): switched on, in the window, on mains power, nobody using the machine.

use bitsbench::rules::{self, Desk, Facts, Power, Settings, Why, decide, in_window, input_came, power_of};

/// 2026-11-14 00:00 UTC.
const DAY: i64 = 20_771 * 86_400;

fn at(h: i64, m: i64) -> i64 {
    // Local time is UTC+1 in the facts below.
    DAY + (h - 1) * 3600 + m * 60
}

fn idle_desk() -> Desk {
    Desk { idle_s: rules::IDLE_S, inputs: 7, fullscreen: false, games: 0, offset_min: 60 }
}

fn facts(now: i64) -> Facts {
    Facts { now_s: now, desk: Some(idle_desk()), power: Power::Mains, rx_bps: Some(0.0), last_done_s: None, sound: false }
}

fn on() -> Settings {
    Settings { on: true, ..Settings::default() }
}

#[test]
fn the_window_holds_its_hours_also_over_midnight() {
    assert!(in_window(2 * 60, 2, 5));
    assert!(in_window(4 * 60 + 59, 2, 5));
    assert!(!in_window(5 * 60, 2, 5));
    assert!(!in_window(60 + 59, 2, 5));
    // 23:00 to 04:00.
    assert!(in_window(23 * 60 + 30, 23, 4));
    assert!(in_window(3 * 60, 23, 4));
    assert!(!in_window(4 * 60, 23, 4));
    assert!(!in_window(22 * 60, 23, 4));
    // Equal hours: never (not all day).
    assert!(!in_window(3 * 60, 3, 3));
    assert_eq!(rules::since_open(60, 23), 120);
}

#[test]
fn on_by_default_and_only_in_the_window() {
    assert_eq!(Settings::default(), Settings { on: true, from_h: 2, to_h: 5 });
    assert_eq!(decide(&Settings::default(), &facts(at(3, 0))), Ok(()));
    assert_eq!(decide(&Settings { on: false, ..Settings::default() }, &facts(at(3, 0))), Err(Why::Off), "Georg switched it off");
    assert_eq!(decide(&on(), &facts(at(3, 0))), Ok(()));
    assert_eq!(decide(&on(), &facts(at(1, 59))), Err(Why::Outside));
    assert_eq!(decide(&on(), &facts(at(5, 0))), Err(Why::Outside));
    assert_eq!(decide(&on(), &facts(at(14, 0))), Err(Why::Outside));
}

#[test]
fn nobody_may_use_the_machine() {
    let mut f = facts(at(3, 0));
    f.desk = Some(Desk { idle_s: rules::IDLE_S - 1, ..idle_desk() });
    assert_eq!(decide(&on(), &f), Err(Why::InUse), "input less than 30 minutes ago");
    f.desk = Some(Desk { fullscreen: true, ..idle_desk() });
    assert_eq!(decide(&on(), &f), Err(Why::FullScreen));
    f.desk = Some(Desk { games: 1, ..idle_desk() });
    assert_eq!(decide(&on(), &f), Err(Why::Game));
    f.desk = Some(idle_desk());
    f.sound = true;
    assert_eq!(decide(&on(), &f), Err(Why::Sound), "a playlist to fall asleep to, a video in a window");
    f.sound = false;
    f.rx_bps = Some(rules::DOWNLOAD_BPS + 1.0);
    assert_eq!(decide(&on(), &f), Err(Why::Download));
    f.rx_bps = None;
    assert_eq!(decide(&on(), &f), Err(Why::Download), "the first minute has no rate yet: wait for it");
    f.rx_bps = Some(1000.0);
    f.power = Power::Battery;
    assert_eq!(decide(&on(), &f), Err(Why::Battery));
    f.power = Power::Unknown;
    assert_eq!(decide(&on(), &f), Ok(()), "no supply says anything: a desktop machine");
    f.desk = None;
    assert_eq!(decide(&on(), &f), Err(Why::NoDesktop), "without bitscomp nobody can say the machine is unused");
}

#[test]
fn once_a_night() {
    let mut f = facts(at(4, 0));
    f.last_done_s = Some(at(2, 40));
    assert_eq!(decide(&on(), &f), Err(Why::DoneTonight));
    f.last_done_s = Some(at(2, 40) - 86_400);
    assert_eq!(decide(&on(), &f), Ok(()), "last night's run does not count");
    // A window over midnight: a run at 23:30 counts for the same night at 01:00.
    let s = Settings { on: true, from_h: 23, to_h: 4 };
    let mut g = facts(at(25, 0));
    g.last_done_s = Some(at(23, 30));
    assert_eq!(decide(&s, &g), Err(Why::DoneTonight));
}

#[test]
fn mains_decides_and_device_batteries_never_count() {
    let s = |t: &str, on: Option<bool>, scope: &str, status: &str| (t.to_string(), on, scope.to_string(), status.to_string());
    assert_eq!(power_of(&[]), Power::Unknown);
    assert_eq!(power_of(&[s("Mains", Some(true), "", "")]), Power::Mains);
    assert_eq!(power_of(&[s("Mains", Some(false), "", ""), s("Battery", None, "", "Discharging")]), Power::Battery);
    assert_eq!(power_of(&[s("Battery", Some(true), "Device", "Discharging")]), Power::Unknown, "a gamepad's battery");
    assert_eq!(power_of(&[s("Battery", None, "", "Discharging")]), Power::Battery);
    assert_eq!(power_of(&[s("Battery", None, "", "Charging")]), Power::Mains);
}

#[test]
fn any_input_stops_a_run() {
    let a = idle_desk();
    assert!(!input_came(&a, &Desk { idle_s: a.idle_s + 60, ..a }));
    assert!(input_came(&a, &Desk { inputs: a.inputs + 1, idle_s: 0, ..a }));
    assert!(input_came(&a, &Desk { inputs: 0, ..a }), "bitscomp started anew");
}

#[test]
fn the_desk_answer_parses() {
    let d = bitsbench::comp::parse_idle("idle input_s=1900 inputs=12 fullscreen=1 games=2 offset_min=60\n").unwrap();
    assert_eq!(d, Desk { idle_s: 1900, inputs: 12, fullscreen: true, games: 2, offset_min: 60 });
    assert!(bitsbench::comp::parse_idle("unknown command 'idle'").is_none());
    let list = "tile=0 page=1/1 visible=true focused=true frame=0,0 10x10 id=4 app_id=steam title=\"Steam\" pid=9\n\
                tile=1 page=1/1 visible=true focused=false frame=0,0 10x10 id=5 app_id=steam_app_1234 title=\"x\" pid=9\nend";
    let steam = |a: &str| bitsbench::bench::steam::is_steam(a, "steam");
    assert_eq!(bitsbench::comp::window_ids(list, &steam), vec![4]);
}

#[test]
fn sound_in_use_is_read_from_the_mixer_and_the_player() {
    use bitsbench::probe::{mixer_clients, music_playing};
    assert_eq!(mixer_clients("mixer=running playing=hdmi clients=1 hdmi=on"), Some(1));
    assert_eq!(mixer_clients("mixer=running playing=none clients=0 hdmi=absent"), Some(0));
    assert_eq!(mixer_clients(""), None, "another Linux has no such line");
    assert!(music_playing(r#"{"ok":true,"state":"playing","playing":true,"list":"Abendliste"}"#));
    assert!(!music_playing(r#"{"ok":true,"state":"stopped","playing":false,"list":""}"#));
    assert!(!music_playing("not json"));
}
