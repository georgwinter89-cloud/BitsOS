//! PM-1: cost of open() for a program (policy hook) next to the kernel's own open (no hook).
use crate::abi;
use crate::inode::Cred;
use crate::ops::Ctx;
use ktest::{kassert, ktest, TestResult};

const N: u64 = 20_000;

fn per_open(c: &Ctx<'_>, p: &[u8]) -> Result<u64, alloc::string::String> {
    for _ in 0..200 {
        kassert!(c.open(None, p, abi::O_RDONLY, 0).is_ok(), "warm-up open failed");
    }
    let t0 = ksync::now_ns().unwrap_or(0);
    for _ in 0..N {
        let f = c.open(None, p, abi::O_RDONLY, 0);
        kassert!(f.is_ok(), "open failed");
        drop(f);
    }
    Ok((ksync::now_ns().unwrap_or(0) - t0) / N)
}

fn open_cost() -> TestResult {
    let ns = crate::root_ns();
    let prog = Ctx { ns: &ns, root: ns.root_path(), cwd: ns.root_path(), cred: Cred { uid: 1000, gid: 1000 }, umask: 0o022, subject: policy::Subject { program: policy::ProgramId(1), pid: 0x7fff_0001, uid: 1000 } };
    let kern = Ctx::kernel(&ns);
    for p in [&b"/dev/null"[..], b"/proc/sys/fs/nr_open"] {
        let k = per_open(&kern, p)?;
        let u = per_open(&prog, p)?;
        klog::event!(Info, "pm", "open_cost", path = core::str::from_utf8(p).unwrap_or("?"), kernel_ns = k, program_ns = u, hook_ns = u.saturating_sub(k));
    }
    Ok(())
}
ktest!("fs.pm_open_cost", Safe, open_cost);

/// The fast path must not skip a program that has a path rule: a deny prefix still refuses its open.
fn deny_prefix() -> TestResult {
    use policy::{AccessClass, Decision, PathRule, RuleSet};
    let id = policy::ProgramId(0x7e57_0b3d);
    let ns = crate::root_ns();
    let prog = Ctx { ns: &ns, root: ns.root_path(), cwd: ns.root_path(), cred: Cred { uid: 1000, gid: 1000 }, umask: 0o022, subject: policy::Subject { program: id, pid: 0x7fff_0002, uid: 1000 } };
    let mut r = RuleSet::allow_all();
    r.paths.push(PathRule { prefix: "/dev/null".into(), modes: policy::rules::modes::READ, decision: Decision::Deny });
    policy::set_rules(id, Some(r));
    let denied = c_open_err(&prog, b"/dev/null");
    let other = prog.open(None, b"/proc/sys/fs/nr_open", abi::O_RDONLY, 0).is_ok();
    let fast = policy::allows_any(&prog.subject, AccessClass::Path);
    policy::set_rules(id, None);
    kassert!(!fast, "allows_any is true for a program with a path rule");
    kassert!(denied == Some(uapi::errno::EACCES), "open under a deny prefix gave {:?}, want EACCES", denied);
    kassert!(other, "open outside the deny prefix failed");
    kassert!(prog.open(None, b"/dev/null", abi::O_RDONLY, 0).is_ok(), "open still refused after the rules were removed");
    Ok(())
}

fn c_open_err(c: &Ctx<'_>, p: &[u8]) -> Option<crate::Errno> {
    c.open(None, p, abi::O_RDONLY, 0).err()
}
ktest!("fs.pm_deny_prefix", Safe, deny_prefix);
