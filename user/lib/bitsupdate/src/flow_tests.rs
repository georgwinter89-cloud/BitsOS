//! `flow::not_newer`: a release at or below the medium's newest folder is "neueste Version" only when the running
//! build is numbered and at least as new; otherwise its slot says why it is not offered.

use crate::flow::{Fail, Installed, Slot, Taken, not_newer};

fn have(running: u32, newest: u32) -> Installed {
    Installed { running, newest, ..Installed::default() }
}

fn judge(v: u32, h: &Installed, s: Option<(u32, &str)>) -> (Result<bool, Fail>, Vec<u32>) {
    let mut asked = Vec::new();
    let r = not_newer(v, h, &mut |n| {
        asked.push(n);
        Ok(s.map(|(version, m)| Slot { version, markers: m.into() }))
    });
    (r.map(|o| o.is_some()), asked)
}

#[test]
fn the_running_numbered_version_is_the_newest() {
    assert_eq!(judge(165, &have(165, 165), None), (Ok(false), vec![]));
    assert_eq!(judge(164, &have(165, 165), None), (Ok(false), vec![]), "an older release");
}

#[test]
fn an_installed_release_that_does_not_run_says_why() {
    // Zurück chosen: 164 runs, 165 is installed and confirmed.
    assert_eq!(judge(165, &have(164, 165), Some((165, "OK"))).0, Err(Fail::Taken(Taken::Back { version: 165, running: 164 })));
    // Installed, not started yet.
    assert_eq!(judge(165, &have(164, 165), Some((165, "NEW"))).0, Err(Fail::Taken(Taken::Waiting(165))));
    // Its one try did not confirm: it fell back.
    assert_eq!(judge(165, &have(164, 165), Some((165, "NEW+TRY"))).0, Err(Fail::Taken(Taken::Failed(165))));
}

#[test]
fn a_development_build_in_the_releases_slot_is_no_newest_version() {
    // The desk's quick install took slot 166 on a medium running 165; then version 166 was published.
    let (r, asked) = judge(166, &have(165, 166), Some((0, "OK")));
    assert_eq!(r, Err(Fail::Taken(Taken::Occupied { version: 166, newest: 166 })));
    assert_eq!(asked, vec![166]);
    // A development build runs from slot 166; version 166 and 165 (pruned) are not offered as "newest".
    assert_eq!(judge(166, &have(0, 166), Some((0, "OK"))).0, Err(Fail::Taken(Taken::Occupied { version: 166, newest: 166 })));
    assert_eq!(judge(165, &have(0, 170), None).0, Err(Fail::Taken(Taken::Occupied { version: 165, newest: 170 })));
}

#[test]
fn a_failing_slot_question_is_a_failure() {
    let r = not_newer(165, &have(0, 166), &mut |_| Err(Fail::Local(String::from("x"))));
    assert_eq!(r.map(|o| o.is_some()), Err(Fail::Local(String::from("x"))));
}

/// GP-R3: the Mini-PC's check ran `bitsupdate status` over MCP; the node refuses that on purpose (EPERM for a
/// process MCP's run started, EACCES for anyone but root) and the line must say so, not "ging etwas schief".
#[test]
fn a_refused_node_says_where_it_works() {
    use std::io::Error;
    for errno in [1, 13] {
        let f = crate::flow::opened(Err(Error::from_raw_os_error(errno))).err().expect("an error");
        assert!(matches!(f, Fail::Refused(_)), "errno {errno}: {f:?}");
        assert!(f.text().contains("nur als root an der Tastatur dieses Rechners"), "{}", f.text());
        assert!(f.detail().is_some_and(|d| d.contains("/dev/bitsos/update")), "{:?}", f.detail());
    }
    let missing = crate::flow::opened(Err(Error::from_raw_os_error(2))).err().expect("an error");
    assert!(matches!(missing, Fail::Local(_)), "ENOENT stays a local failure: {missing:?}");
}
