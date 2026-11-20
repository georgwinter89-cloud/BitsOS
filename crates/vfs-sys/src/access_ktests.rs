//! DB2 ktest `access.hooks.probe`: `/bin/access-probe` (user/c/access-probe.c) reads a file, makes, renames and
//! removes a file and a folder, listens, connects to itself and sends 5 bytes, and opens a device node; the access
//! log's text must name exactly that for its pid, with its program line.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use ktest::{TestResult, kassert, ktest};

const SEC: u64 = 1_000_000_000;
const PROG: &str = "/bin/access-probe";
/// Device nodes the probe tries in this order (not mem or tty: the log leaves those out); it says which opened.
const DEVICES: &str = "/dev/snd/pcmC0D0c,/dev/ntsync,/dev/fb0";
static RUN: AtomicU64 = AtomicU64::new(0);

fn run(dir: &str, input: &str, dev: &str) -> Result<(u32, String), String> {
    let argv = ["access-probe", input, dir, dev];
    let env = ["HOME=/", "PATH=/bin"];
    let out = proc::stdio::Capture::new(1 << 12);
    let mut o = proc::SpawnOptions::new(&argv, &env, policy::ProgramId(1));
    o.stdout = Some(out.clone() as Arc<dyn vfs::node::File>);
    let p = proc::spawn_with(PROG, &o).map_err(|e| format!("spawn: {e}"))?;
    p.wait(Some(30 * SEC));
    let said = String::from_utf8_lossy(&out.contents()).into_owned();
    match p.status() {
        Some(proc::ExitStatus::Exited(0)) => Ok((p.pid, said)),
        Some(s) => Err(format!("probe ended {s:?}: {said:?}")),
        None => {
            p.kill(9);
            p.wait(Some(5 * SEC));
            Err(format!("probe still ran after 30 s: {said:?}"))
        }
    }
}

fn probe() -> TestResult {
    let ns = vfs::root_ns();
    kassert!(ns.lookup(PROG).is_ok(), "{PROG} is not in the image");
    let dir = format!("/tmp/db2-probe-{}", RUN.fetch_add(1, Ordering::Relaxed));
    // 0777: the probe may run as another uid than root.
    ns.mkdir_p_bytes(dir.as_bytes(), 0o777).map_err(|e| format!("mkdir {dir}: {e}"))?;
    let input = format!("{dir}/eingabe.txt");
    ns.create_file(&input, 0o644, b"was drin steht\n".to_vec()).map_err(|e| format!("create {input}: {e}"))?;
    // Held: bitsaccess (on a live system) neither takes the probe's records nor loses its own to this test.
    let hold = vfs::access::drain::hold();
    let was = vfs::access::on();
    vfs::access::set_on(true);
    let ran = run(&dir, &input, DEVICES);
    let text = match &ran {
        Ok((pid, _)) => hold.text(hold.take(|r| r.pid == *pid || (r.kind == vfs::access::Kind::Fork && matches!(r.obj, vfs::access::Obj::Num(c) if c == *pid as u64)))),
        Err(_) => String::new(),
    };
    vfs::access::set_on(was);
    drop(hold);
    let (pid, said) = ran?;
    let port = said.lines().find_map(|l| l.strip_prefix("port ")).ok_or_else(|| format!("no port line: {said:?}"))?.trim();
    kassert!(said.contains("done"), "probe said {said:?}");
    let dev = said.lines().find_map(|l| l.strip_prefix("device ")).ok_or_else(|| format!("no device line: {said:?}"))?.trim();
    let mark = format!("\"pid\":{pid},");
    let mine: Vec<&str> = text.lines().filter(|l| l.contains(&mark)).collect();
    let has = |want: &str| mine.iter().any(|l| l.contains(want));
    let want = [
        format!("\"k\":\"read\",\"dir\":\"{dir}\""),
        // The file was renamed and removed before the read: the name it was made with.
        format!("\"k\":\"create\",\"dir\":\"{dir}\",\"name\":\"neu.txt\",\"n\":2"),
        format!("\"k\":\"rename\",\"dir\":\"{dir}\",\"n\":1"),
        format!("\"k\":\"unlink\",\"dir\":\"{dir}\",\"n\":2"),
        format!("\"k\":\"exec\",\"path\":\"{PROG}\""),
        format!("\"k\":\"listen\",\"proto\":\"tcp\",\"addr\":\"127.0.0.1\",\"port\":{port},\"n\":1"),
        format!("\"k\":\"connect\",\"proto\":\"tcp\",\"addr\":\"127.0.0.1\",\"port\":{port},\"n\":1"),
        "\"k\":\"accept\",\"proto\":\"tcp\",\"addr\":\"127.0.0.1\"".into(),
        format!("\"k\":\"netclose\",\"proto\":\"tcp\",\"addr\":\"127.0.0.1\",\"port\":{port},\"tx\":5,\"rx\":0"),
        "\"tx\":0,\"rx\":5".into(),
        format!("\"k\":\"device\",\"path\":\"{dev}\",\"w\":false"),
    ];
    for w in &want {
        kassert!(has(w), "missing {w} in the probe's lines:\n{}", mine.join("\n"));
    }
    kassert!(text.contains(&format!("{{\"p\":{pid},")) && text.contains(&format!("\"exe\":\"{PROG}\"")), "no program line for pid {pid}:\n{text}");
    kassert!(!mine.iter().any(|l| l.contains("\"k\":\"write\"")), "a created file also counted as written:\n{}", mine.join("\n"));
    Ok(())
}
ktest!("access.hooks.probe", Safe, probe);
