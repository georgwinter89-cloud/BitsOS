//! DB5a: Durchblick's bridge to foreign programs on V2, with its fake program (real programs are checked only on
//! the Mini-PC). atspid starts as root as init starts it: it binds C3, hands the socket to uid 1000 and drops to
//! that user, then runs the accessibility bus (busd) and the registry. The fake program (uid 1000) announces itself;
//! `atspid tree` and `atspid act` as root read its window and press, type and check through C3; as uid 1000 (not
//! the desktop) they get only the German refusal, which shows that the kernel's peer label reaches atspid.
//! QemuAll: the fake is a test program, which the lean stick image leaves out; the Mini-PC has its own check list
//! with the real programs.

use super::{MS, SEC, SIGKILL, finish, need, scratch, start_as, text, wait_for};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};

fn client_as(uid: u32, sock: &str, args: &[&str]) -> Result<(i32, String), String> {
    let mut argv = alloc::vec!["atspid", args[0], "--socket", sock];
    argv.extend_from_slice(&args[1..]);
    let s = start_as("/bin/atspid", &argv, None, false, Some(uid))?;
    let code = finish(&s, 30 * SEC)?;
    Ok((code, s.both()))
}

fn client(sock: &str, args: &[&str]) -> Result<(i32, String), String> {
    client_as(0, sock, args)
}

fn atspi() -> TestResult {
    need("/bin/atspid")?;
    need("/bin/atspid-fake")?;
    let dir = scratch("atspi");
    let ns = vfs::root_ns();
    ns.mkdir_p_bytes(dir.as_bytes(), 0o777).map_err(|e| format!("mkdir {dir}: {e}"))?;
    let (bus, sock) = (format!("{dir}/bus"), format!("{dir}/c3.sock"));
    let d = start_as("/bin/atspid", &["atspid", "serve", "--no-settings", "--bus", &bus, "--socket", &sock], None, false, Some(0))?;
    let mut fake = None;
    let r = (|| -> TestResult {
        let up = wait_for(20 * SEC, || client(&sock, &["ping"]).is_ok_and(|(c, o)| c == 0 && o.contains("\"running\": true")));
        kassert!(up, "atspid never ran its bus:\n{}", d.both());
        let f = start_as("/bin/atspid-fake", &["atspid-fake", "--bus", &bus], None, false, Some(1000))?;
        let embedded = wait_for(20 * SEC, || text(&f.out).contains("embedded under :"));
        kassert!(embedded, "the fake program did not get through Embed:\n{}\n--- atspid ---\n{}", f.both(), d.both());
        let pid = format!("{}", f.p.pid);
        fake = Some(f);
        let (c, out) = client_as(1000, &sock, &["tree", &pid])?;
        kassert!(c != 0 && out.contains("nur für den Schreibtisch") && !out.contains("Formular"), "uid 1000 (not the desktop) was answered ({c}):\n{out}");
        let (c, out) = client(&sock, &["tree", &pid])?;
        kassert!(c == 0, "tree exited {c}:\n{out}");
        for want in ["\"title\": \"Formular\"", "\"source\": \"atspi\"", "\"Passwort\"", "\"secret\"", "\"value\": \"Georg\"", "\"Senden\""] {
            kassert!(out.contains(want), "the tree lacks {want}:\n{out}");
        }
        kassert!(!out.contains("geheim") && !out.contains("Versteckt"), "a password or a hidden element in the tree:\n{out}");
        // The command line's surface origin and size (the Mini-PC check list uses them): twice the program's size.
        let (c, big) = client(&sock, &["tree", "--origin", "100,50", "--size", "1600,1200", &pid])?;
        kassert!(c == 0 && big.contains("1600,") && big.contains("1200") && big.contains("300,"), "origin/size not applied ({c}):\n{big}");
        let id = |n: u32| format!("atspi-{pid}/{n}");
        let acts: [(u32, &str, Option<&str>); 3] = [(2, "type", Some("Hallo")), (4, "toggle", None), (9, "press", None)];
        for (n, a, t) in acts {
            let mut args: Vec<&str> = alloc::vec!["act", &pid];
            let el = id(n);
            args.push(&el);
            args.push(a);
            if let Some(t) = t {
                args.push(t);
            }
            let (c, out) = client(&sock, &args)?;
            kassert!(c == 0 && out.contains("\"ok\": true"), "act {el} {a} exited {c}:\n{out}");
        }
        let (c, out) = client(&sock, &["tree", &pid])?;
        kassert!(c == 0 && out.contains("Status: gedrückt Senden mit Hallo") && out.contains("\"checked\""), "the acts did not show:\n{out}");
        kassert!(!d.p.has_exited(), "atspid ended: {:?}\n{}", d.p.status(), d.both());
        klog::event!(Info, "ul", "test.atspi", tree_bytes = out.len());
        Ok(())
    })();
    if let Some(f) = fake {
        f.p.kill(SIGKILL);
        let _ = f.p.wait(Some(10 * SEC));
    }
    d.p.kill(SIGKILL);
    let _ = d.p.wait(Some(10 * SEC));
    sched::sleep_ns(10 * MS);
    r
}
ktest!("db.atspi", QemuAll, atspi);
