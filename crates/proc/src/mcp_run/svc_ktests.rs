//! MC-6 ktests with a real user program (prefix `mc.svc.user_`): `/bin/mcp-fakesvc` (user/lib/mcpreg, the client
//! library) registers through AF_UNIX on a registry of the test's own (`/run/bitsos/ktest-<port>.sock`, given in
//! BITSOS_MCP_SOCK); calls go over a loopback MCP server of the test's own. Safe and rerunnable: every program a
//! test starts is ended and waited for before it returns.

use super::Stream;
use crate::Process;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use ktest::{TestResult, kassert, ktest};
use mcp::testclient::{Client, Services, with_services};
use vfs::File;

const SEC: u64 = 1_000_000_000;
const FAKE: &str = "/bin/mcp-fakesvc";

/// Starts the fake service (uid 1000, as `run` does) with its stdout in a stream.
fn start(s: &Services, args: &[&str]) -> Result<(Arc<Process>, Arc<Stream>), String> {
    let mut argv = alloc::vec![FAKE];
    argv.extend_from_slice(args);
    let env = format!("BITSOS_MCP_SOCK={}", s.path);
    let envp = [env.as_str()];
    let out = Stream::new(64 * 1024);
    let mut o = crate::SpawnOptions::new(&argv, &envp, super::MCP_PROGRAM);
    o.stdout = Some(out.clone() as Arc<dyn File>);
    let p = crate::spawn_with(FAKE, &o).map_err(|e| format!("spawn {FAKE}: {e}"))?;
    Ok((p, out))
}

fn text(out: &Stream) -> String {
    String::from_utf8_lossy(&out.contents()).into_owned()
}

fn end(p: &Process) {
    p.kill(uapi::ab::signal::SIGKILL);
    let _ = p.wait(Some(5 * SEC));
}

fn until(what: &str, secs: u64, mut cond: impl FnMut() -> bool) -> TestResult {
    let end = time::now_ns() + secs * SEC;
    while !cond() {
        if time::now_ns() >= end {
            return Err(format!("{what}: not within {secs} s"));
        }
        sched::sleep_ns(10_000_000);
    }
    Ok(())
}

/// The fake must be in the image (it is, whenever the user workspace was built).
fn present() -> Option<TestResult> {
    let ns = vfs::root_ns();
    vfs::Ctx::kernel(&ns).lookup(None, FAKE.as_bytes(), true).is_err().then(|| ktest::skip("/bin/mcp-fakesvc is not in this image (user/lib/mcpreg not built)"))
}

/// Runs `f` with a started fake; ends the fake on every way out.
fn with_fake(port: u16, args: &[&str], f: impl FnOnce(&mut Client, &Services, &Arc<Process>, &Stream) -> TestResult) -> TestResult {
    if let Some(r) = present() {
        return r;
    }
    with_services(port, mcp::TOOLS.entries(), |_| {}, |c, s| {
        let (p, out) = start(s, args)?;
        let r = f(c, s, &p, &out);
        end(&p);
        r
    })
}

fn user_echo() -> TestResult {
    with_fake(25270, &["echo", "ufake"], |c, s, p, out| {
        until("registered", 10, || s.reg.tool_count() == 2)?;
        until("stdout", 5, || text(out).contains("registered 2"))?;
        let t = c.ok("ufake_echo", r#"{"n":3,"text":"ä \"q\""}"#)?;
        kassert!(t == r#"echo {"n":3,"text":"ä \"q\""}"#, "answer {t}");
        kassert!(c.ok("ufake_wipe", "{}")? == "wiped nothing", "wipe");
        let svcs = s.reg.services();
        kassert!(svcs.len() == 1 && svcs[0].0 == "ufake" && svcs[0].1 == p.pid && svcs[0].2 == 2, "services {svcs:?}");
        // The program ends (SIGTERM): its tools vanish.
        p.kill(uapi::ab::signal::SIGTERM);
        kassert!(p.wait(Some(5 * SEC)).is_some(), "the fake did not end");
        until("tools gone", 5, || s.reg.tool_count() == 0)?;
        let r = c.call("ufake_echo", "{}")?;
        kassert!(r.rpc_error == Some(-32602), "a vanished tool is unknown");
        Ok(())
    })
}
ktest!("mc.svc.user_echo", Safe, user_echo);

fn user_hang() -> TestResult {
    with_fake(25290, &["hang", "uhang"], |c, s, _p, out| {
        until("registered", 10, || s.reg.tool_count() == 1)?;
        let r = c.call("uhang_hang", r#"{"timeout_ms":400}"#)?;
        kassert!(r.error && r.text.contains("did not answer uhang_hang within"), "{}", r.text);
        until("cancel reached the program", 5, || text(out).contains(" timeout"))?;
        kassert!(c.ok("version", "{}")?.starts_with("BitsOS V2"), "version");
        kassert!(s.reg.tool_count() == 1, "a hung service keeps its tools");
        Ok(())
    })
}
ktest!("mc.svc.user_hang", Safe, user_hang);

fn user_crash() -> TestResult {
    with_fake(25310, &["crash", "ucrash"], |c, s, p, _out| {
        until("registered", 10, || s.reg.tool_count() == 1)?;
        let r = c.call("ucrash_crash", r#"{"timeout_ms":5000}"#)?;
        kassert!(r.error && r.text.contains("ended before it answered"), "{}", r.text);
        kassert!(p.wait(Some(5 * SEC)) == Some(128 + 6), "the fake ended by SIGABRT: {:?}", p.exit_code());
        until("tools gone", 5, || s.reg.tool_count() == 0)?;
        Ok(())
    })
}
ktest!("mc.svc.user_crash", Safe, user_crash);

fn user_many() -> TestResult {
    with_fake(25330, &["many", "umany", "100"], |c, s, _p, _out| {
        until("registered", 10, || s.reg.tool_count() == 100)?;
        let t = c.ok("umany_t042", r#"{"n":42}"#)?;
        kassert!(t == r#"echo {"n":42}"#, "{t}");
        Ok(())
    })
}
ktest!("mc.svc.user_many", Safe, user_many);

fn user_bad_schema() -> TestResult {
    if let Some(r) = present() {
        return r;
    }
    with_services(25350, mcp::TOOLS.entries(), |_| {}, |_c, s| {
        let (p, _out) = start(s, &["bad", "ubad"])?;
        let code = p.wait(Some(10 * SEC));
        if code.is_none() {
            end(&p);
        }
        kassert!(code == Some(3), "the refused program exits 3: {code:?}");
        kassert!(s.reg.tool_count() == 0, "nothing registered");
        Ok(())
    })
}
ktest!("mc.svc.user_bad_schema", Safe, user_bad_schema);

/// A service that forks: the child holds the socket (no EOF comes), the parent exits; its tools still vanish
/// (the watchdog asks the process table), and a call in between fails instead of running into its deadline.
fn user_fork() -> TestResult {
    with_fake(25370, &["fork", "ufork"], |_c, s, p, out| {
        until("registered", 10, || s.reg.tool_count() == 1)?;
        let code = p.wait(Some(10 * SEC));
        let child = text(out).lines().find_map(|l| l.strip_prefix("child ").and_then(|n| n.trim().parse::<u32>().ok()));
        let r = (|| {
            kassert!(code == Some(0), "the parent exits 0: {code:?}");
            let Some(cpid) = child else { return Err(format!("no child pid: {}", text(out))) };
            kassert!(crate::find(cpid).is_some_and(|c| !c.is_finished()), "the child holds the socket");
            until("tools gone though the child holds the socket", 5, || s.reg.tool_count() == 0)?;
            Ok(())
        })();
        if let Some(c) = child.and_then(crate::find) {
            end(&c);
        }
        r
    })
}
ktest!("mc.svc.user_fork", Safe, user_fork);
