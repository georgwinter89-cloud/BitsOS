//! `xtask test --interim`: the gate without the stable tests of `host/tests/stable.txt`, for the runs between the
//! integrator's full runs. Every build and the warnings check still run, and the summary line says what was left out.
use crate::ktest::KtestOpts;
use crate::stable::{self, Stable};
use crate::util::*;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

pub struct Interim {
    pub list: Stable,
    /// The ktest names every boot leaves out.
    pub exclude: Vec<String>,
    /// How many of them the tree's sources still have (the others are tests that no longer exist).
    exclude_live: usize,
    host: usize,
    boots: Vec<String>,
    scenario: Vec<String>,
    step: Vec<String>,
}

impl Interim {
    pub fn load(root: &Path) -> Result<Interim, String> {
        let list = stable::load(root)?;
        let src = stable::source_names(root);
        let exclude = stable::boot_exclude(root, &list);
        let exclude_live = exclude.iter().filter(|n| src.contains(*n)).count();
        println!(
            "interim: {} lists {} host, {} ktest ({} left out by the boots), {} scenario, {} step",
            stable::FILE,
            list.host.len(),
            list.ktest.len(),
            exclude.len(),
            list.scenario.len(),
            list.step.len()
        );
        Ok(Interim { list, exclude, exclude_live, host: 0, boots: vec![], scenario: vec![], step: vec![] })
    }

    /// The arguments that leave the stable tests out of a cargo test step: `list` is that step's command, which runs here
    /// with `--tests -- --list` first (the same test binaries, no doc tests, nothing run). A failed listing leaves nothing out.
    pub fn host_args(&mut self, root: &Path, name: &str, mut list: Command) -> Vec<String> {
        let file = xdir(root).join(format!("interim-list-{}.txt", name.replace(|c: char| !c.is_ascii_alphanumeric(), "-")));
        let _ = std::fs::create_dir_all(xdir(root));
        // One file for both streams: cargo's `Running` lines (stderr) must stay in front of their binary's list (stdout).
        let listed = std::fs::File::create(&file).and_then(|f| {
            let g = f.try_clone()?;
            list.args(["--tests", "--", "--list"]).stdout(f).stderr(g).status()
        });
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        if !listed.is_ok_and(|s| s.success()) {
            println!("interim: {name}: listing its tests failed ({}), every test runs", file.display());
            return Vec::new();
        }
        let (args, n) = stable::host_skips(&text, &self.list);
        self.host += n;
        println!("interim: {name}: {n} stable test(s) left to the integrator's full run");
        if args.is_empty() { args } else { std::iter::once("--".to_string()).chain(args).collect() }
    }

    /// A ktest boot as the interim gate starts it: without the stable names, and not at all when its filter has nothing else.
    pub fn boot(&mut self, name: &str, mut o: KtestOpts) -> Option<KtestOpts> {
        o.filters.retain(|f| !stable::runs_nothing(f, &self.exclude));
        if o.filters.is_empty() {
            println!("interim: {name}: every test of this boot is stable, the boot is left to the integrator's full run");
            self.boots.push(name.to_string());
            return None;
        }
        o.exclude = self.exclude.clone();
        Some(o)
    }

    /// The scripts the interim gate starts. A script a test of this run's boots names always runs (that test is not
    /// stable, or not left out); of the others (`ALWAYS_IN_GATE`) a stable one is left out.
    pub fn scripts(&mut self, wanted: BTreeSet<String>, named: &BTreeSet<String>) -> BTreeSet<String> {
        let (keep, left): (BTreeSet<String>, BTreeSet<String>) = wanted.into_iter().partition(|s| named.contains(s) || !self.list.scenario.contains(s));
        for s in &left {
            println!("interim: scenario {s}: stable, left to the integrator's full run");
        }
        self.scenario.extend(left);
        keep
    }

    /// True when the gate step `name` is listed: it is not started.
    pub fn leaves_step(&mut self, name: &str) -> bool {
        let left = stable::step_can_be_left_out(name) && self.list.step.contains(name);
        if left {
            println!("xtask: {name}: stable, left to the integrator's full run");
            self.step.push(name.to_string());
        }
        left
    }

    /// The number of stable tests this run left out and what they were.
    pub fn line(&self) -> String {
        let n = self.host + self.exclude_live + self.scenario.len() + self.step.len();
        let mut s = format!(
            "{n} stable tests left to the integrator's full run ({} host, {} ktest, {} scenario, {} step",
            self.host,
            self.exclude_live,
            self.scenario.len(),
            self.step.len()
        );
        if !self.boots.is_empty() {
            s.push_str(&format!("; boots not started: {}", self.boots.join(", ")));
        }
        s.push(')');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::BuildOpts;

    fn interim(list: &str) -> Interim {
        let list = stable::parse(list).unwrap();
        let exclude = list.ktest.iter().cloned().collect::<Vec<_>>();
        let exclude_live = exclude.len();
        Interim { list, exclude, exclude_live, host: 0, boots: vec![], scenario: vec![], step: vec![] }
    }

    fn opts(filters: &[&str]) -> KtestOpts {
        KtestOpts { filters: filters.iter().map(|f| f.to_string()).collect(), timeout: 1, build: BuildOpts::default(), net: "none".into(), extra: vec![], config: vec![], exclude: vec![] }
    }

    #[test]
    fn boots_lose_the_stable_names_or_are_not_started() {
        let mut i = interim("ktest:rb.crash.nmi_nested\nktest:mm.paging.a\n");
        let o = i.boot("ktest", opts(&[""])).unwrap();
        assert_eq!(o.exclude, ["mm.paging.a", "rb.crash.nmi_nested"]);
        assert!(i.boot("ktest nmi-nested", opts(&["rb.crash.nmi_nested"])).is_none());
        let o = i.boot("ktest paging smp8", opts(&["mm.paging.a", "mm.paging.b"])).unwrap();
        assert_eq!(o.filters, ["mm.paging.b"]);
        assert!(i.line().starts_with("2 stable tests left to the integrator's full run (0 host, 2 ktest, 0 scenario, 0 step; boots not started: ktest nmi-nested)"), "{}", i.line());
    }

    #[test]
    fn only_unnamed_listed_scripts_and_listed_steps_are_left_out() {
        let mut i = interim("scenario:rb/20-reload\nscenario:nv/10-nvme\nstep:wy gles\n");
        let wanted: BTreeSet<String> = ["rb/20-reload", "nv/10-nvme", "fs/30-fat"].map(String::from).into();
        let named: BTreeSet<String> = ["nv/10-nvme", "fs/30-fat"].map(String::from).into();
        assert_eq!(i.scripts(wanted, &named).into_iter().collect::<Vec<_>>(), ["fs/30-fat", "nv/10-nvme"]);
        assert!(i.leaves_step("wy gles") && !i.leaves_step("wy dmabuf") && !i.leaves_step("warnings"));
        assert!(i.line().starts_with("2 stable tests left"), "{}", i.line());
    }
}
