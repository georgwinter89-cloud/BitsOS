//! MC-10: the full-access store with a fake clock: expiry, a start after a reboot, revoke, only real input and
//! only paired clients, hostile files; the decision for `request_access`; paired clients read from DER files.
use super::*;
use crate::decide::{Confirmer, Outcome, State};
use crate::grant::Grants;
use crate::store::FileKeep;
use mcp_wire::Json;
use mcp_wire::installer::{Asked, Kind};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize};
use std::time::Duration;

const T: i64 = 1_800_000_000;

/// One fake clock per test (tests run in parallel).
fn fake() -> (Arc<AtomicI64>, Arc<AtomicI64>, Clock) {
    let (w, m) = (Arc::new(AtomicI64::new(T)), Arc::new(AtomicI64::new(100)));
    let (cw, cm) = (w.clone(), m.clone());
    (w, m, Box::new(move || (cw.load(Ordering::SeqCst), cm.load(Ordering::SeqCst))))
}

fn fp(c: char) -> String {
    c.to_string().repeat(64)
}

fn paired() -> Box<dyn Fn() -> Vec<Client> + Send + Sync> {
    Box::new(|| vec![Client { fp: fp('a'), name: "bitsos-desk".into() }, Client { fp: fp('b'), name: "laptop".into() }])
}

fn dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("bitsinstall-access-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn store(d: &std::path::Path) -> Option<Box<dyn Keep>> {
    Some(Box::new(FileKeep::in_dir(d)))
}

fn load(d: &std::path::Path) -> Vec<Grant> {
    from_text(&FileKeep::in_dir(d).load_text(Duration::ZERO).unwrap().unwrap_or_default()).0
}

#[test]
fn a_grant_holds_for_its_time_and_survives_a_restart() {
    let d = dir("restart");
    let (w, m, clock) = fake();
    let a = Access::new(Vec::new(), store(&d), clock, paired());
    assert!(a.take_dirty(), "the kernel gets the (empty) table at the start");
    let g = a.grant(&fp('a'), 1, true, Input::Physical).unwrap();
    assert_eq!((g.until, g.set_at, g.nvme), (T + 3600, T, true));
    assert!(a.take_dirty() && !a.take_dirty(), "one push per change");
    assert_eq!(a.shown()[0].name, "bitsos-desk");
    assert_eq!(fs_mode(&d.join("grants")), 0o600);
    // A reboot 10 minutes later: the file is read again and judged from the new start.
    w.store(T + 600, Ordering::SeqCst);
    m.store(5, Ordering::SeqCst);
    let (_, _, clock2) = fake();
    let again = Access::new(load(&d), store(&d), clock2, paired());
    assert_eq!(again.table(), vec![g.clone()], "kept on the volume");
    // Its end by the wall clock.
    w.store(T + 3599, Ordering::SeqCst);
    m.store(3599 - 600 + 5, Ordering::SeqCst);
    assert!(!a.sweep());
    w.store(T + 3600, Ordering::SeqCst);
    m.store(3600 - 600 + 5, Ordering::SeqCst);
    assert!(a.sweep() && a.table().is_empty() && a.take_dirty());
    assert!(load(&d).is_empty(), "the end is kept too");
    let _ = std::fs::remove_dir_all(&d);
}

fn fs_mode(p: &std::path::Path) -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).unwrap().mode() & 0o777
}

#[test]
fn only_real_input_only_paired_clients_only_the_choices() {
    let (_, _, clock) = fake();
    let a = Access::new(Vec::new(), None, clock, paired());
    assert!(a.grant(&fp('a'), 24, false, Input::Injected).unwrap_err().contains("injected"));
    assert!(a.grant(&fp('a'), 24, false, Input::None).is_err());
    assert!(a.grant(&fp('c'), 24, false, Input::Physical).unwrap_err().contains("no paired client"));
    for h in [0, 2, 25, 169, -1] {
        assert!(a.grant(&fp('a'), h, false, Input::Physical).is_err(), "{h} h");
    }
    for h in mcp_wire::access::CHOICES_H {
        assert_eq!(a.grant(&fp('a'), h, false, Input::Physical).unwrap().until, T + h * 3600);
    }
    assert_eq!(a.table().len(), 1, "a new grant replaces the client's old one");
    a.grant(&fp('b'), 8, false, Input::Physical).unwrap();
    assert_eq!(a.revoke(), 2);
    assert!(a.table().is_empty() && a.revoke() == 0);
}

#[test]
fn the_clock_jumping_or_going_back_ends_a_grant() {
    let (w, m, clock) = fake();
    let a = Access::new(Vec::new(), None, clock, paired());
    a.grant(&fp('a'), 1, false, Input::Physical).unwrap();
    // Set back by 20 minutes after 30: the monotonic end still counts (not stretched).
    w.store(T + 600, Ordering::SeqCst);
    m.store(100 + 1800, Ordering::SeqCst);
    assert!(!a.sweep());
    m.store(100 + 3600, Ordering::SeqCst);
    assert!(a.sweep(), "ended at its monotonic end although the wall clock says 50 min");
    // A jump forward by more than its length.
    a.grant(&fp('a'), 1, false, Input::Physical).unwrap();
    w.store(T + 600 + 3601 + 5, Ordering::SeqCst);
    m.store(100 + 3600 + 5, Ordering::SeqCst);
    assert!(a.sweep() && a.table().is_empty());
    // Back before the moment it was set.
    w.store(T, Ordering::SeqCst);
    a.grant(&fp('b'), 24, false, Input::Physical).unwrap();
    w.store(T - 10, Ordering::SeqCst);
    assert!(a.sweep());
}

/// The file is input (another machine can write the stick's data image): never a panic, never more than
/// well-formed grants of at most 7 days, each client once; what does not hold now is dropped at the start.
#[test]
fn a_hostile_file_gives_no_grant() {
    let ok = format!("{} {} {} 0\n", fp('a'), T + 60, T);
    for bad in [
        String::from("\u{0}\u{ff}"),
        format!("{} {} {}\n", fp('a'), T + 60, T),
        format!("{} {} {} 2\n", fp('a'), T + 60, T),
        format!("{} {} {} 0\n", fp('A'), T + 60, T),
        format!("{} {} {} 0\n", &fp('a')[1..], T + 60, T),
        format!("{} {} {} 0\n", fp('a'), T + 8 * 86_400, T),
        format!("{} {} {} 0\n", fp('a'), i64::MAX, i64::MIN),
        format!("{} {} {} 0\n", fp('a'), T, T + 1),
        format!("{} x y 0 extra\n", fp('a')),
    ] {
        let (v, n) = from_text(&bad);
        assert!(v.is_empty() && n >= 1, "{bad:?}");
    }
    let (v, n) = from_text(&format!("{ok}{ok}"));
    assert_eq!((v.len(), n), (1, 1), "a client twice");
    // Well formed but set ten years ahead or long over: dropped when taken.
    let ahead = T + 10 * 365 * 86_400;
    let (v, _) = from_text(&format!("{} {} {} 1\n{} {} {} 0\n", fp('a'), ahead + 60, ahead, fp('b'), T - 10, T - 100));
    assert_eq!(v.len(), 2);
    let (_, _, clock) = fake();
    let a = Access::new(v, None, clock, paired());
    assert!(a.table().is_empty(), "nothing that does not hold now is taken");
    assert_eq!(to_text(&[Grant { client: fp('a'), until: T + 60, set_at: T, nvme: true }]), format!("{} {} {T} 1\n", fp('a'), T + 60));
}

struct Fake {
    out: Outcome,
    asked: AtomicUsize,
    changed: AtomicUsize,
}
impl Confirmer for Fake {
    fn ask(&self, _: u64, _: Kind, _: &str, _: &str, _: Duration) -> Outcome {
        panic!("the access dialog, not the plain one")
    }
    fn connected(&self) -> bool {
        true
    }
    fn ask_access(&self, _: u64, c: &Client, hours: i64, title: &str, text: &str, t: Duration) -> Outcome {
        assert!(title.contains(&c.name) && title.contains(&mcp_wire::access::fp_short(&c.fp)), "{title}");
        assert!(text.contains(&format!("{hours} h")) && text.contains("Strg+Alt+V"), "{text}");
        assert!(t >= Duration::from_secs(240), "Georg has to walk to the keyboard");
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.out
    }
    fn access_changed(&self) {
        self.changed.fetch_add(1, Ordering::SeqCst);
    }
}

fn request(client: &str) -> Asked {
    Asked { id: 7, tool: "request_access".into(), kind: Kind::Access, args: Json::parse(r#"{"hours":24,"reason":"Welle 21"}"#).unwrap(), timeout_ms: 5000, peer: "10.0.2.2:4000".into(), client: client.into() }
}

#[test]
fn request_access_is_decided_by_the_dialogs_choice() {
    let (w, _, clock) = fake();
    let a = Arc::new(Access::new(Vec::new(), None, clock, paired()));
    let s = State::new(Grants::default(), None, Duration::from_secs(5)).with_access(a.clone());
    let f = |out| Fake { out, asked: AtomicUsize::new(0), changed: AtomicUsize::new(0) };
    let yes = f(Outcome::Access { hours: 8, nvme: true, input: Input::Physical });
    let d = s.decide(&yes, &request(&fp('a')), T);
    assert_eq!((d.yes, d.by.as_str()), (true, "dialog"), "{d:?}");
    assert_eq!(a.table(), vec![Grant { client: fp('a'), until: T + 8 * 3600, set_at: T, nvme: true }]);
    assert_eq!(yes.changed.load(Ordering::SeqCst), 1);
    for (out, by) in [
        (Outcome::Access { hours: 24, nvme: false, input: Input::Injected }, "refused"),
        (Outcome::Answer { yes: true, input: Input::Physical }, "refused"),
        (Outcome::Answer { yes: false, input: Input::Physical }, "dialog"),
        (Outcome::Timeout, "timeout"),
        (Outcome::Ended, "no_session"),
    ] {
        // Past the cool-down a refusal leaves (the fake clock moves on).
        w.fetch_add(crate::decide::ACCESS_COOL_S + 1, Ordering::SeqCst);
        let d = s.decide(&f(out), &request(&fp('b')), T);
        assert_eq!((d.yes, d.by.as_str()), (false, by), "{out:?}");
    }
    assert_eq!(a.table().len(), 1, "no grant for b");
    // Right after a refusal the same client gets no dialog for a while; then one again.
    w.fetch_add(crate::decide::ACCESS_COOL_S + 1, Ordering::SeqCst);
    assert_eq!(s.decide(&f(Outcome::Answer { yes: false, input: Input::Physical }), &request(&fp('b')), T).by, "dialog");
    let held_back = f(Outcome::Access { hours: 24, nvme: false, input: Input::Physical });
    let d = s.decide(&held_back, &request(&fp('b')), T);
    assert!(!d.yes && d.by == "refused" && d.reason.contains("within 60 s"), "{d:?}");
    assert_eq!(held_back.asked.load(Ordering::SeqCst), 0, "no dialog during the cool-down");
    w.fetch_add(crate::decide::ACCESS_COOL_S + 1, Ordering::SeqCst);
    assert!(s.decide(&held_back, &request(&fp('b')), T).yes, "after the cool-down Georg is asked again");
    // A key that is no paired client is refused before any dialog.
    let never = f(Outcome::Access { hours: 1, nvme: false, input: Input::Physical });
    assert_eq!(s.decide(&never, &request(&fp('c')), T).by, "refused");
    assert_eq!(never.asked.load(Ordering::SeqCst), 0);
}

fn tlv(tag: u8, parts: &[&[u8]]) -> Vec<u8> {
    let body = parts.concat();
    let mut v = vec![tag, 0x82, (body.len() >> 8) as u8, body.len() as u8];
    v.extend_from_slice(&body);
    v
}

fn cert(cn: &str) -> Vec<u8> {
    let name = tlv(0x30, &[&tlv(0x31, &[&tlv(0x30, &[&tlv(0x06, &[&[0x55, 4, 3]]), &tlv(0x0c, &[cn.as_bytes()])])])]);
    let alg = tlv(0x30, &[&tlv(0x06, &[&[0x2a, 0x86, 0x48, 0xce, 0x3d, 4, 3, 2]])]);
    let tbs = tlv(0x30, &[&tlv(0xa0, &[&tlv(0x02, &[&[2]])]), &tlv(0x02, &[&[1; 8]]), &alg, &name, &tlv(0x30, &[]), &name, &tlv(0x30, &[&[0; 64]])]);
    tlv(0x30, &[&tbs, &alg, &tlv(0x03, &[&[0, 9]])])
}

#[test]
fn paired_clients_are_read_from_the_stick() {
    let d = dir("paired");
    std::fs::create_dir_all(&d).unwrap();
    let desk = cert("bitsos-desk");
    std::fs::write(d.join("first.der"), &desk).unwrap();
    std::fs::write(d.join("second.der"), cert("laptop")).unwrap();
    std::fs::write(d.join("first.bad"), cert("cut")).unwrap();
    std::fs::write(d.join("junk.der"), b"not a certificate").unwrap();
    std::fs::write(d.join("big.der"), vec![0u8; 20_000]).unwrap();
    let c = clients_in(&d);
    let names: Vec<&str> = c.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(c.len(), 2, "{names:?}");
    assert!(names.contains(&"bitsos-desk") && names.contains(&"laptop"));
    assert!(c.iter().any(|x| x.fp == sha256_hex(&desk)));
    assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    assert!(clients_in(&d.join("missing")).is_empty());
    let _ = std::fs::remove_dir_all(&d);
}

/// A store that fails while `down` is set (the data volume unmounted, full or read-only).
struct Flaky {
    down: Arc<std::sync::atomic::AtomicBool>,
    text: Arc<std::sync::Mutex<String>>,
}
impl Keep for Flaky {
    fn save(&self, text: &str) -> Result<(), String> {
        if self.down.load(Ordering::SeqCst) {
            return Err(String::from("read-only"));
        }
        *self.text.lock().unwrap() = String::from(text);
        Ok(())
    }
}

fn flaky() -> (Arc<std::sync::atomic::AtomicBool>, Arc<std::sync::Mutex<String>>, Box<dyn Keep>) {
    let (d, t) = (Arc::new(std::sync::atomic::AtomicBool::new(false)), Arc::new(std::sync::Mutex::new(String::new())));
    (d.clone(), t.clone(), Box::new(Flaky { down: d, text: t }))
}

/// Georg revokes while /data does not take the write: the kernel's table loses the grant at once, the stick
/// notes the revoke, the next start does not take the old file's grant, and the save is tried again until it
/// is written (then the note is cleared).
#[test]
fn a_revoke_the_volume_did_not_take_still_holds() {
    let (w, _, clock) = fake();
    let (down, text, keep) = flaky();
    let (_, tomb_text, tomb) = flaky();
    let a = Access::new(Vec::new(), Some(keep), clock, paired()).with_tombstone(tomb, false);
    a.grant(&fp('a'), 24, true, Input::Physical).unwrap();
    let before = text.lock().unwrap().clone();
    assert!(before.starts_with(&format!("hw {T}\n")) && before.contains(&fp('a')), "{before}");
    let _ = a.take_dirty();
    down.store(true, Ordering::SeqCst);
    w.fetch_add(600, Ordering::SeqCst);
    assert_eq!(a.revoke(), 1);
    assert!(a.table().is_empty() && a.take_dirty(), "the kernel is told at once");
    assert!(a.unsaved(), "the save is owed");
    let tomb_now = tomb_text.lock().unwrap().clone();
    assert_eq!(revoked_at(&tomb_now), Some(T + 600), "{tomb_now}");
    // A start now (the old file still holds the grant): the tombstone keeps it out.
    let (old, _) = from_text(&before);
    assert_eq!(old.len(), 1);
    assert!(restore(old.clone(), high_water(&before), revoked_at(&tomb_now), T + 700).is_empty(), "the revoked grant does not come back");
    assert_eq!(restore(old, high_water(&before), None, T + 700).len(), 1, "without the note it would");
    // The volume is back: the next sweep writes the empty table and clears the note.
    down.store(false, Ordering::SeqCst);
    a.sweep();
    assert!(!a.unsaved());
    assert!(from_text(&text.lock().unwrap()).0.is_empty());
    assert_eq!(revoked_at(&tomb_text.lock().unwrap()), None, "the note is cleared once the volume holds the revoke");
}

/// The wall clock's high-water mark is written while a grant is live; a start with the clock set back by more
/// than the slack (BIOS, a Windows dual boot writing local time) takes no grant, a forward jump only shortens it.
#[test]
fn a_clock_set_back_across_a_start_ends_the_grants() {
    let (w, m, clock) = fake();
    let (_, text, keep) = flaky();
    let a = Access::new(Vec::new(), Some(keep), clock, paired());
    a.grant(&fp('a'), 1, false, Input::Physical).unwrap();
    w.fetch_add(HIGH_WATER_EVERY_S, Ordering::SeqCst);
    m.fetch_add(HIGH_WATER_EVERY_S, Ordering::SeqCst);
    a.sweep();
    let t = text.lock().unwrap().clone();
    assert_eq!(high_water(&t), Some(T + HIGH_WATER_EVERY_S), "the mark moves on while the grant is live");
    let (list, _) = from_text(&t);
    assert_eq!(restore(list.clone(), high_water(&t), None, T + 2 * HIGH_WATER_EVERY_S).len(), 1, "a normal start");
    assert_eq!(restore(list.clone(), high_water(&t), None, T).len(), 1, "within the slack");
    assert!(restore(list.clone(), high_water(&t), None, T + HIGH_WATER_EVERY_S - HIGH_WATER_SLACK_S - 1).is_empty(), "two hours back: no grant");
    assert!(restore(list.clone(), high_water(&t), None, T - 7200).is_empty());
    assert_eq!(restore(list, None, None, T - 7200).len(), 1, "an old file without a mark: only the clock rule");
}

/// Only a pairing Georg confirmed at the keyboard is recorded; `confirmed` is read leniently.
#[test]
fn a_confirmed_pairing_is_recorded() {
    let (_, _, clock) = fake();
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let s2 = seen.clone();
    let a = Access::new(Vec::new(), None, clock, paired()).with_pairing_record(Box::new(move |fp| {
        s2.lock().unwrap().push(fp.into());
        Ok(())
    }));
    a.confirm_pairing(&fp('a'));
    a.confirm_pairing("not a key");
    assert_eq!(*seen.lock().unwrap(), vec![fp('a')]);
    let t = with_confirmed(&with_confirmed("garbage\n\n", &fp('b')), &fp('a'));
    assert_eq!(confirmed_in(&t), vec![fp('a'), fp('b')]);
    assert_eq!(confirmed_in(&with_confirmed(&t, &fp('a'))).len(), 2, "no key twice");
    // decide: a yes to `pair_client` records the fingerprint it named; a no records nothing.
    let s = State::new(Grants::default(), None, Duration::from_secs(5)).with_access(Arc::new(a));
    struct Yes(bool);
    impl Confirmer for Yes {
        fn ask(&self, _: u64, _: Kind, _: &str, _: &str, _: Duration) -> Outcome {
            Outcome::Answer { yes: self.0, input: Input::Physical }
        }
        fn connected(&self) -> bool {
            true
        }
    }
    let pair = |c: char| Asked { id: 9, tool: "pair_client".into(), kind: Kind::Pair, args: Json::parse(&format!(r#"{{"fingerprint":"{}","name":"x"}}"#, fp(c))).unwrap(), timeout_ms: 5000, peer: "10.0.2.2:4000".into(), client: String::new() };
    assert!(!s.decide(&Yes(false), &pair('c'), T).yes);
    assert!(s.decide(&Yes(true), &pair('d'), T).yes);
    assert_eq!(*seen.lock().unwrap(), vec![fp('a'), fp('d')]);
}
