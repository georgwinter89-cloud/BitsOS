//! GP-START: the stick busy with the kernel's own big write (`/run/bitsos/data-state` says `creating` while data.img is
//! made) is no hung stick: the fallback says so in a quiet line, never warns while it waits, touches the stick only
//! once the write ended, carries the changes made meanwhile over and says so in one plain line. A hang without that
//! write still warns (tests/store.rs `hung_*`).

mod common;
use common::*;
use settingsd::json::Json;

fn data_state(env: &Env, line: &str) {
    std::fs::write(env.paths.data_state(), line).unwrap();
}

fn lines(ev: &std::sync::Arc<std::sync::Mutex<Vec<String>>>) -> Vec<String> {
    ev.lock().unwrap().clone()
}

fn no_alarm(ev: &std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let l = lines(ev);
    assert!(!l.iter().any(|l| l.contains("hung?") || l.contains("resync.failed") || l.contains("stuck=")), "{l:?}");
}

fn back_line(ev: &std::sync::Arc<std::sync::Mutex<Vec<String>>>, keys: usize) {
    let back: Vec<String> = lines(ev).into_iter().filter(|l| l.starts_with("settingsd back_on_stick ")).collect();
    assert_eq!(back.len(), 1, "{back:?}");
    let plain = "Einstellungen sind jetzt dort gespeichert";
    assert!(back[0].contains(&format!("keys={keys} ")) && back[0].contains(plain), "{back:?}");
}

#[test]
fn a_start_while_data_img_is_made_waits_quietly_and_carries_over() {
    let env = Env::new("busy-start", true);
    std::fs::create_dir_all(env.paths.stick.join("settings")).unwrap();
    // The read at the start waits behind the write (a FIFO opened for reading blocks until a writer comes).
    let fifo = env.paths.stick.join("settings/theme.toml");
    mkfifo(&fifo);
    data_state(&env, "creating\n");
    let (mut r, ev) = env.open();
    assert_eq!(r.status().str("place"), Some("fallback"));
    assert!(r.status().str("reason").is_some_and(|w| w.starts_with("busy: ")), "{:?}", r.status().str("reason"));
    assert!(lines(&ev).iter().any(|l| l.starts_with("settingsd fallback reason=\"busy: ")), "{:?}", lines(&ev));
    r.set("theme.radius", &Json::Int(9), &WHO).unwrap();
    // However often the ticker comes while the write goes on: no try, no warning.
    for _ in 0..3 {
        assert!(!r.tick_now());
    }
    no_alarm(&ev);
    // The write ends: the stuck read comes back, the kernel's line says mounted.
    drop(std::fs::OpenOptions::new().write(true).open(&fifo).unwrap());
    std::fs::remove_file(&fifo).unwrap();
    data_state(&env, "mounted pool rw created\n");
    assert!(until(10, || {
        r.tick_now();
        r.status().str("place") == Some("stick")
    }));
    assert!(env.stick_file("settings/theme.toml").contains("theme.radius = 9"));
    no_alarm(&ev);
    back_line(&ev, 1);
}

#[test]
fn a_write_that_waits_behind_data_img_moves_aside_quietly() {
    let env = Env::new("busy-write", true);
    let (mut r, ev) = env.open();
    assert_eq!(r.status().str("place"), Some("stick"));
    std::fs::create_dir_all(env.paths.stick.join("settings")).unwrap();
    // Opening a FIFO for writing blocks until a reader comes: the write waits behind data.img.
    let fifo = env.paths.stick.join("settings/theme.toml.new");
    mkfifo(&fifo);
    data_state(&env, "creating\n");
    let a = r.set("theme.radius", &Json::Int(5), &WHO).unwrap();
    assert_eq!(a.str("stored"), Some("fallback"));
    assert!(r.status().str("reason").is_some_and(|w| w.starts_with("busy: ")));
    r.set("theme.radius", &Json::Int(6), &WHO).unwrap();
    assert!(!r.tick_now());
    no_alarm(&ev);
    let _ = std::fs::File::open(&fifo);
    data_state(&env, "mounted pool rw created\n");
    assert!(until(15, || {
        r.tick_now();
        r.status().str("place") == Some("stick")
    }));
    assert!(env.stick_file("settings/theme.toml").contains("theme.radius = 6"));
    no_alarm(&ev);
    back_line(&ev, 1);
}

/// The Mini-PC's case: /data lies elsewhere (`mounted ...` from the start), a slow stick is a hang as ever.
#[test]
fn without_the_kernels_write_a_slow_stick_still_warns() {
    let env = Env::new("busy-not", true);
    std::fs::create_dir_all(env.paths.stick.join("settings")).unwrap();
    let fifo = env.paths.stick.join("settings/theme.toml");
    mkfifo(&fifo);
    data_state(&env, "mounted pool rw\n");
    let (r, ev) = env.open();
    assert!(r.status().str("reason").is_some_and(|w| w.starts_with("slow: ")));
    assert!(lines(&ev).iter().any(|l| l.starts_with("settingsd fallback ") && l.contains("hung?")));
    drop(std::fs::OpenOptions::new().write(true).open(&fifo).unwrap());
}

fn count(ev: &std::sync::Arc<std::sync::Mutex<Vec<String>>>, start: &str) -> usize {
    lines(ev).iter().filter(|l| l.starts_with(start)).count()
}

/// Unblocks a reader stuck on `fifo` and puts a fresh FIFO in its place (the next read hangs again).
fn release(fifo: &std::path::Path, again: bool) {
    drop(std::fs::OpenOptions::new().write(true).open(fifo).unwrap());
    std::fs::remove_file(fifo).unwrap();
    if again {
        mkfifo(fifo);
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
}

/// A data.img that is still made after the limit: the stick counts as hung for the rest of that creation, the
/// quiet wait never starts again (checker finding 1).
#[test]
fn past_the_limit_the_stick_stays_hung_while_data_img_is_made() {
    let env = Env::new("busy-max", true);
    std::fs::create_dir_all(env.paths.stick.join("settings")).unwrap();
    let fifo = env.paths.stick.join("settings/theme.toml");
    mkfifo(&fifo);
    data_state(&env, "creating\n");
    let (mut r, ev) = env.open();
    r.set_busy_max(std::time::Duration::from_millis(800));
    assert!(r.status().str("reason").is_some_and(|w| w.starts_with("busy: ")));
    std::thread::sleep(std::time::Duration::from_millis(900));
    // The first worker comes back; the next try hangs again while the kernel still writes.
    release(&fifo, true);
    assert!(r.tick_now(), "past the limit the stick is tried");
    assert_eq!(count(&ev, "settingsd resync.failed "), 1, "{:?}", lines(&ev));
    assert!(r.status().str("reason").is_some_and(|w| w.starts_with("slow: ")), "{:?}", r.status().str("reason"));
    for _ in 0..5 {
        r.tick_now();
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(count(&ev, "settingsd resync.waiting reason=busy"), 0, "{:?}", lines(&ev));
    assert!(r.status().str("reason").is_some_and(|w| w.starts_with("slow: ")));
    release(&fifo, false);
}

/// The kernel is done but the stick then refuses for another reason (pulled out): that is said as a warning, and
/// the later return claims no data.img wait (checker finding 6).
#[test]
fn a_refusal_after_data_img_is_warned_and_not_blamed_on_it() {
    let env = Env::new("busy-other", true);
    std::fs::create_dir_all(env.paths.stick.join("settings")).unwrap();
    let fifo = env.paths.stick.join("settings/theme.toml");
    mkfifo(&fifo);
    data_state(&env, "creating\n");
    let (mut r, ev) = env.open();
    r.set("theme.radius", &Json::Int(7), &WHO).unwrap();
    release(&fifo, false);
    data_state(&env, "mounted pool rw created\n");
    let stick = env.paths.stick.clone();
    let aside = env.root.join("stick-out");
    std::fs::rename(&stick, &aside).unwrap();
    assert!(until(15, || {
        r.tick_now();
        count(&ev, "settingsd fallback reason=") == 2
    }));
    let l = lines(&ev);
    assert!(l.iter().any(|l| l.starts_with("settingsd fallback reason=") && l.contains("no stick")), "{l:?}");
    assert!(r.status().str("reason").is_some_and(|w| w.contains("no stick")));
    std::fs::rename(&aside, &stick).unwrap();
    assert!(until(15, || {
        r.tick_now();
        r.status().str("place") == Some("stick")
    }));
    assert!(env.stick_file("settings/theme.toml").contains("theme.radius = 7"));
    assert_eq!(count(&ev, "settingsd back_on_stick "), 0, "{:?}", lines(&ev));
}
