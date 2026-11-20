//! PK-6c: the resolver against pacman 7.1 on the build host's databases (scratch copies, uid 1000, nothing
//! installed) and on small hand-made indexes.
mod common;

use bitspkg::repo::index::Index;
use bitspkg::repo::resolve::policy::{self, Prefs};
use bitspkg::repo::resolve::{dep, Closure, Plan, Rule, Universe};
use bitspkg::repo::sources::{self, Level};
use bitspkg::repo::syncdb::{self, Pkg};
use common::tmp;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

const REPOS: [&str; 6] = ["cachyos-v3", "cachyos-extra-v3", "cachyos-core-v3", "cachyos", "core", "extra"];

struct Host {
    dir: PathBuf,
    idx: Vec<(String, Index)>,
}

fn host() -> &'static Host {
    static H: OnceLock<Host> = OnceLock::new();
    H.get_or_init(|| {
        assert_ne!(usys::getuid().unwrap(), 0, "runs as uid 1000 (pacman on a scratch copy, never as root)");
        let names: Vec<String> = sources::defaults("test").sources.iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, REPOS, "the scratch pacman.conf must name the default sources in their order");
        let dir = tmp("resolve-host");
        for d in ["db/sync", "db/local", "cache", "gpg"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        let mut conf = String::from("[options]\nArchitecture = x86_64 x86_64_v3\nSigLevel = Never\n");
        let mut idx = Vec::new();
        for r in REPOS {
            let to = dir.join("db/sync").join(format!("{r}.db"));
            std::fs::copy(format!("/var/lib/pacman/sync/{r}.db"), &to).unwrap_or_else(|e| panic!("{r}.db from the build host: {e}"));
            conf.push_str(&format!("[{r}]\nServer = http://127.0.0.1:1/$repo\n"));
            let db = syncdb::parse(std::fs::File::open(&to).unwrap(), r).unwrap_or_else(|e| panic!("{r}: {e}"));
            idx.push((r.to_string(), Index { db_sha256: String::new(), newest: db.newest(), pkgs: db.pkgs }));
        }
        std::fs::write(dir.join("pacman.conf"), conf).unwrap();
        // One pacman run before the parallel ones: it sets up the empty local database (ALPM_DB_VERSION), and
        // several first runs at once race on that.
        let o = Command::new("/usr/bin/pacman").arg("--config").arg(dir.join("pacman.conf")).arg("--dbpath").arg(dir.join("db")).args(["-Sl", "core"]).output().expect("/usr/bin/pacman");
        assert!(o.status.success(), "pacman -Sl core: {}", String::from_utf8_lossy(&o.stderr));
        Host { dir, idx }
    })
}

fn uni() -> &'static Universe<'static> {
    static U: OnceLock<Universe<'static>> = OnceLock::new();
    U.get_or_init(|| Universe::new(&host().idx, Level::V3))
}

/// `pacman -Sp` on the scratch databases: (repo, name, version) in pacman's install order.
fn pacman_sp(name: &str) -> Result<Vec<(String, String, String)>, String> {
    let d = &host().dir;
    let o = Command::new("/usr/bin/pacman")
        .arg("--config")
        .arg(d.join("pacman.conf"))
        .arg("--dbpath")
        .arg(d.join("db"))
        .arg("--cachedir")
        .arg(d.join("cache"))
        .arg("--gpgdir")
        .arg(d.join("gpg"))
        .args(["--logfile", "/dev/null", "--noconfirm", "-Sp", "--print-format", "PKG %r %n %v", name])
        .output()
        .expect("/usr/bin/pacman");
    if !o.status.success() {
        return Err(String::from_utf8_lossy(&o.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.strip_prefix("PKG ")?.split(' ');
            Some((it.next()?.to_string(), it.next()?.to_string(), it.next()?.to_string()))
        })
        .collect())
}

fn ours(p: &Plan) -> Vec<(String, String, String)> {
    p.packages.iter().map(|x| (x.repo.clone(), x.name.clone(), x.version.clone())).collect()
}

fn programs() -> Vec<String> {
    let f = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../host/tests/pk/closure-50.txt");
    let t = std::fs::read_to_string(&f).unwrap();
    t.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(str::to_string).collect()
}

/// Each package after its dependencies, unless the later one (transitively) needs the earlier: a cycle.
fn check_order(u: &Universe, p: &Plan) {
    let ids: Vec<usize> = p.packages.iter().map(|x| u.entry(&x.repo, &x.name).unwrap()).collect();
    let deps_of = |i: usize| -> Vec<usize> { (0..ids.len()).filter(|&j| u.entries[ids[i]].depends.iter().any(|d| u.sat(ids[j], d))).collect() };
    let graph: Vec<Vec<usize>> = (0..ids.len()).map(deps_of).collect();
    let reaches = |from: usize, to: usize| {
        let mut seen = HashSet::new();
        let mut st = vec![from];
        while let Some(x) = st.pop() {
            if x == to {
                return true;
            }
            if seen.insert(x) {
                st.extend(graph[x].iter().copied());
            }
        }
        false
    };
    for (i, ds) in graph.iter().enumerate() {
        for &j in ds {
            if j > i {
                assert!(reaches(j, i), "{}: {} comes before its dependency {} without a cycle", p.target, p.packages[i].name, p.packages[j].name);
            }
        }
    }
}

#[test]
fn closures_of_50_programs_equal_pacman() {
    let list = programs();
    assert_eq!(list.len(), 50, "closure-50.txt");
    assert_eq!(list.iter().collect::<BTreeSet<_>>().len(), 50, "closure-50.txt has a name twice");
    let u = uni();
    let want: Vec<(String, Result<Vec<(String, String, String)>, String>)> = std::thread::scope(|s| {
        let hs: Vec<_> = list.chunks(7).map(|c| s.spawn(move || c.iter().map(|n| (n.clone(), pacman_sp(n))).collect::<Vec<_>>())).collect();
        hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    let mut differ = 0;
    let mut total = 0;
    for (name, w) in &want {
        let w = w.as_ref().unwrap_or_else(|e| panic!("pacman -Sp {name}: {e}"));
        let raw = Closure::pacman(name, u);
        assert!(raw.ok(), "{name}: {:?}", raw.refused);
        assert_eq!(ours(&raw).iter().collect::<BTreeSet<_>>(), w.iter().collect::<BTreeSet<_>>(), "{name}: closure (repo, name, version) differs from pacman -Sp");
        assert_eq!(ours(&raw), *w, "{name}: install order differs from pacman -Sp");
        check_order(u, &raw);
        total += w.len();
        // BitsOS's rules with an empty table must give pacman's closure: then the table is the only cause of a
        // difference below.
        let bare = Closure::resolve(name, u, &Prefs::none(), &[]);
        assert!(bare.ok(), "{name} refused: {:?}", bare.refused);
        assert_eq!(ours(&bare).iter().collect::<BTreeSet<_>>(), w.iter().collect::<BTreeSet<_>>(), "{name}: BitsOS's rules without the table differ from pacman -Sp");
        // With BitsOS's table: each difference must come from a table choice, named here.
        let plan = Closure::resolve(name, u, &Prefs::builtin(), &[]);
        assert!(plan.ok(), "{name} refused: {:?}", plan.refused);
        check_order(u, &plan);
        let a: BTreeSet<_> = ours(&plan).into_iter().collect();
        let b: BTreeSet<_> = w.iter().cloned().collect();
        if a != b {
            differ += 1;
            let table: Vec<String> = plan.choices.iter().filter(|c| c.rule == Rule::Table).map(|c| format!("{} -> {} (pacman: {})", c.dep, c.chosen, c.first)).collect();
            assert!(!table.is_empty(), "{name}: differs from pacman without a table choice");
            println!("{name}: anders als pacman wegen der Vorgabe {}: mehr {:?}, weniger {:?}", table.join(", "), a.difference(&b).map(|x| &x.1).collect::<Vec<_>>(), b.difference(&a).map(|x| &x.1).collect::<Vec<_>>());
        }
        for c in &plan.choices {
            assert!(!c.others.is_empty(), "{name}: a choice without alternatives is no choice");
        }
    }
    println!("50 programs, {total} packages equal to pacman -Sp; {differ} differ by the provider table");
}

#[test]
fn system_parts_and_non_programs_are_refused() {
    let u = uni();
    for n in ["linux", "linux-firmware", "systemd", "grub", "mkinitcpio", "pacman", "nvidia-dkms", "sddm", "linux-cachyos"] {
        let p = Closure::resolve(n, u, &Prefs::builtin(), &[]);
        assert!(!p.ok() && p.packages.is_empty(), "{n} must be refused");
        assert!(p.refused[0].1.starts_with("Das ist ein Systemteil"), "{n}: {:?}", p.refused);
        println!("{n}: {}", p.refused[0].1);
    }
    // multilib-only: steam itself, and a real package whose dependency only multilib has.
    let p = Closure::resolve("steam", u, &Prefs::builtin(), &[]);
    assert!(p.refused[0].1.contains("keiner eingeschalteten Quelle"), "{:?}", p.refused);
    let p = Closure::resolve("umu-launcher", u, &Prefs::builtin(), &[]);
    assert!(p.refused.iter().any(|(n, r)| n.starts_with("lib32-") && r.contains("multilib")), "umu-launcher: {:?}", p.refused);
    println!("umu-launcher: {:?}", p.refused);
    for n in ["libx11", "libxcb", "ttf-dejavu", "noto-fonts", "glib2-docs", "linux-api-headers"] {
        let p = Closure::resolve(n, u, &Prefs::builtin(), &[]);
        assert!(!p.ok(), "{n} must be refused");
        println!("{n}: {}", p.refused[0].1);
    }
    let p = Closure::resolve("archlinux-keyring", u, &Prefs::builtin(), &[]);
    assert!(p.ok(), "the keyring is allowed: {:?}", p.refused);
    let p = Closure::resolve("systemd-libs", u, &Prefs::builtin(), &[]);
    assert!(p.refused.iter().all(|(_, r)| !r.contains("Systemteil")), "systemd-libs is no system part: {:?}", p.refused);
    let p = Closure::resolve("nosuchprogram", u, &Prefs::builtin(), &[]);
    assert!(p.refused[0].1.contains("keiner eingeschalteten Quelle"));
    let p = Closure::resolve("multilib/steam", u, &Prefs::builtin(), &[]);
    assert!(p.refused[0].1.contains("nicht eingeschaltet"));
}

#[test]
fn every_dependency_string_of_the_host_databases_parses() {
    let mut n = 0;
    let mut bad = Vec::new();
    for (r, i) in &host().idx {
        for p in i.pkgs.values() {
            for s in p.depends.iter().chain(&p.provides).chain(&p.conflicts).chain(&p.replaces).chain(&p.optdepends) {
                n += 1;
                if let Err(e) = dep::parse(s) {
                    bad.push(format!("{r}/{}: {e}", p.name));
                }
            }
        }
    }
    assert!(bad.is_empty(), "{} of {n} unreadable: {:?}", bad.len(), &bad[..bad.len().min(10)]);
    assert!(uni().entries.iter().all(|e| e.broken.is_none()));
    println!("{n} dependency strings parse");
}

// ---- hand-made indexes ----

fn pkg(name: &str, ver: &str, f: &[(&str, &[&str])]) -> Pkg {
    let mut p = Pkg { name: name.into(), version: ver.into(), arch: "x86_64".into(), csize: 1000, isize: 3000, ..Default::default() };
    for (k, v) in f {
        let v: Vec<String> = v.iter().map(|s| s.to_string()).collect();
        match *k {
            "depends" => p.depends = v,
            "provides" => p.provides = v,
            "conflicts" => p.conflicts = v,
            "replaces" => p.replaces = v,
            "optdepends" => p.optdepends = v,
            "makedepends" => p.makedepends = v,
            "arch" => p.arch = v[0].clone(),
            "desc" => p.desc = v[0].clone(),
            "groups" => p.groups = v,
            _ => panic!("{k}"),
        }
    }
    p
}

fn idx(repos: Vec<(&str, Vec<Pkg>)>) -> Vec<(String, Index)> {
    repos.into_iter().map(|(r, ps)| (r.to_string(), Index { db_sha256: String::new(), newest: 0, pkgs: ps.into_iter().map(|p| (p.name.clone(), p)).collect() })).collect()
}

fn names(p: &Plan) -> Vec<String> {
    p.packages.iter().map(|x| x.name.clone()).collect()
}

#[test]
fn provides_versions_sonames_and_virtual_packages() {
    let i = idx(vec![
        ("one", vec![pkg("dash", "0.5-1", &[("provides", &["sh"])]), pkg("foo-nover", "1-1", &[("provides", &["foo"])]), pkg("app", "1-1", &[("depends", &["foo>=2", "libcurl.so=4-64", "sh", "lib"])])]),
        ("two", vec![pkg("bash", "5-1", &[("provides", &["sh"])]), pkg("foo-two", "2-1", &[("provides", &["foo=2"])]), pkg("curl", "8-1", &[("provides", &["libcurl.so=4-64"])]), pkg("lib", "1:2.0-1", &[])]),
        ("three", vec![pkg("lib-provider", "1-1", &[("provides", &["lib=9"])])]),
    ]);
    let u = Universe::new(&i, Level::V3);
    let p = Closure::pacman("app", &u);
    assert!(p.ok(), "{:?}", p.refused);
    // foo>=2: the unversioned provide does not count; libcurl.so by its soname; sh: pacman's first is dash.
    assert_eq!(names(&p).iter().collect::<BTreeSet<_>>(), ["app", "foo-two", "curl", "dash", "lib"].iter().map(|s| s.to_string()).collect::<BTreeSet<_>>().iter().collect());
    assert_eq!(p.choices.len(), 1);
    assert_eq!((p.choices[0].chosen.as_str(), p.choices[0].rule), ("dash", Rule::First));
    // The table takes bash for sh and says so.
    let q = Closure::resolve("app", &u, &Prefs::builtin(), &[]);
    assert!(names(&q).contains(&"bash".to_string()) && !names(&q).contains(&"dash".to_string()));
    assert_eq!(q.choices[0].rule, Rule::Table);
    assert_eq!(q.choices[0].others, vec!["one/dash"]);
    assert_eq!(*names(&q).last().unwrap(), "app", "the program comes last");
    println!("{}", q.render());
}

#[test]
fn previous_provider_newest_java_and_literal_before_provider() {
    let i = idx(vec![
        ("one", vec![pkg("p1", "1-1", &[("provides", &["impl"])]), pkg("p2", "1-1", &[("provides", &["impl"])]), pkg("jre17-openjdk", "17-1", &[("provides", &["java-runtime=17"])]), pkg("aaa-jre", "1-1", &[("provides", &["java-runtime=30"])]), pkg("x-prov", "1-1", &[("provides", &["x"])])]),
        ("two", vec![pkg("jre-openjdk", "25-1", &[("provides", &["java-runtime=25"])]), pkg("jre21-openjdk", "21-1", &[("provides", &["java-runtime=21"])]), pkg("x", "1-1", &[]), pkg("app", "1-1", &[("depends", &["impl", "java-runtime>=17", "x"])])]),
    ]);
    let u = Universe::new(&i, Level::V3);
    let p = Closure::resolve("app", &u, &Prefs::builtin(), &["p2".to_string()]);
    assert!(p.ok(), "{:?}", p.refused);
    let n = names(&p);
    assert!(n.contains(&"p2".into()) && !n.contains(&"p1".into()), "{n:?}");
    assert!(n.contains(&"jre-openjdk".into()), "newest jre*-openjdk, not aaa-jre: {n:?}");
    assert!(n.contains(&"x".into()) && !n.contains(&"x-prov".into()), "a later source's literal beats an earlier provider");
    let rules: HashMap<&str, Rule> = p.choices.iter().map(|c| (c.chosen.as_str(), c.rule)).collect();
    assert_eq!(rules["p2"], Rule::Previous);
    assert_eq!(rules["jre-openjdk"], Rule::Table);
}

#[test]
fn conflicts_and_replaces_inside_one_closure() {
    let i = idx(vec![("one", vec![pkg("app", "1-1", &[("depends", &["b", "c"])]), pkg("b", "1-1", &[("conflicts", &["c"])]), pkg("c", "1-1", &[])])]);
    let u = Universe::new(&i, Level::V3);
    let p = Closure::resolve("app", &u, &Prefs::builtin(), &[]);
    assert!(!p.ok() && p.refused[0].1.contains("verträgt sich nicht mit c"), "{:?}", p.refused);
    // c conflicts with b and provides it: b leaves, app's need for b is met by c (sync.c:543).
    let i = idx(vec![("one", vec![pkg("app", "1-1", &[("depends", &["b", "c"])]), pkg("b", "1-1", &[]), pkg("c", "1-1", &[("conflicts", &["b"]), ("provides", &["b"]), ("replaces", &["b"])])])]);
    let u = Universe::new(&i, Level::V3);
    let p = Closure::resolve("app", &u, &Prefs::builtin(), &["b".to_string()]);
    assert!(p.ok(), "{:?}", p.refused);
    assert_eq!(names(&p), ["c", "app"]);
    assert!(p.notes.iter().any(|n| n.contains("b fällt weg")), "{:?}", p.notes);
    assert!(p.notes.iter().any(|n| n.contains("c ersetzt b aus der vorigen Version")), "{:?}", p.notes);
    // %REPLACES% removes nothing in a fresh closure (sync.c:124 acts on installed packages only): a provides and
    // replaces b without a conflict, both stay, as pacman -Sp keeps both.
    let i = idx(vec![("one", vec![pkg("app", "1-1", &[("depends", &["a", "b>=1"])]), pkg("a", "1-1", &[("provides", &["b"]), ("replaces", &["b"])]), pkg("b", "1-1", &[])])]);
    let u = Universe::new(&i, Level::V3);
    for p in [Closure::resolve("app", &u, &Prefs::builtin(), &[]), Closure::pacman("app", &u)] {
        assert!(p.ok(), "{:?}", p.refused);
        assert_eq!(names(&p).iter().collect::<BTreeSet<_>>(), ["a", "app", "b"].iter().map(|s| s.to_string()).collect::<BTreeSet<_>>().iter().collect());
    }
    assert!(Closure::resolve("app", &u, &Prefs::builtin(), &[]).notes.iter().any(|n| n.contains("a ersetzt b, beide")));
    // Two programs may use different versions of one library (D8): each closure is alone.
    let i = idx(vec![("one", vec![pkg("lib", "2-1", &[]), pkg("new", "1-1", &[("depends", &["lib>=2"])])]), ("two", vec![pkg("lib", "1-1", &[]), pkg("old", "1-1", &[("depends", &["lib<2"])])])]);
    let u = Universe::new(&i, Level::V3);
    let a = Closure::resolve("new", &u, &Prefs::builtin(), &[]);
    let b = Closure::resolve("old", &u, &Prefs::builtin(), &[]);
    assert_eq!(a.packages[0].version, "2-1");
    assert_eq!((b.packages[0].repo.as_str(), b.packages[0].version.as_str()), ("two", "1-1"));
}

#[test]
fn arch_level_optdepends_cycles_and_a_deep_chain() {
    let i = idx(vec![("one", vec![pkg("fast", "1-1", &[("arch", &["x86_64_v4"])]), pkg("app", "1-1", &[("depends", &["fast"])]), pkg("any", "1-1", &[("arch", &["any"])]), pkg("tool", "1-1", &[("depends", &["any", "a"]), ("optdepends", &["any: schon dabei", "extra-thing: for more"])]), pkg("a", "1-1", &[("depends", &["b"])]), pkg("b", "1-1", &[("depends", &["a"])])])]);
    let u = Universe::new(&i, Level::V3);
    let p = Closure::resolve("app", &u, &Prefs::builtin(), &[]);
    assert!(p.refused.iter().any(|(_, r)| r.contains("nur für x86_64_v4")), "D2: {:?}", p.refused);
    assert!(Closure::resolve("app", &Universe::new(&i, Level::V4), &Prefs::builtin(), &[]).ok());
    let p = Closure::resolve("tool", &u, &Prefs::builtin(), &[]);
    assert!(p.ok(), "{:?}", p.refused);
    assert!(!names(&p).contains(&"extra-thing".into()));
    assert_eq!(p.optional.len(), 2);
    assert!(p.optional[0].present && !p.optional[1].present && p.optional[1].reason == "for more");
    assert_eq!(*names(&p).last().unwrap(), "tool");
    assert!(p.render().contains("Zusätze") && p.render().contains("extra-thing: for more"));
    // 20000 packages in one chain: no recursion, no stack overflow.
    let n = 20000;
    let chain: Vec<Pkg> = (0..n).map(|k| if k + 1 < n { pkg(&format!("c{k}"), "1-1", &[("depends", &[format!("c{}", k + 1).as_str()])]) } else { pkg(&format!("c{k}"), "1-1", &[]) }).collect();
    let i = idx(vec![("one", chain)]);
    let u = Universe::new(&i, Level::V3);
    let p = Closure::pacman("c0", &u);
    assert_eq!(p.packages.len(), n);
    assert_eq!(p.packages[0].name, format!("c{}", n - 1));
}

#[test]
fn hostile_dependency_strings_are_refused_with_a_reason() {
    let long = "a".repeat(70 << 10);
    for s in ["", ">=", "<>", "foo>=", "foo=", "=1", "foo>=1 2", "foo<=>1", "fo\u{0}o", "foo\n>=1", "-foo", ".foo", "foo bar", long.as_str(), "ä>=1", "foo>=1<2"] {
        let e = dep::parse(s).expect_err(s);
        assert!(!e.is_empty() && e.len() < 400, "{s:?}: {e}");
    }
    for (s, n, v) in [("foo>=1:2.3-1", "foo", "1:2.3-1"), ("libcurl.so=4-64", "libcurl.so", "4-64"), ("python: for scripts", "python", ""), ("a<2", "a", "2"), ("x>1", "x", "1"), ("y<=3", "y", "3")] {
        let d = dep::parse(s).unwrap();
        assert_eq!((d.name.as_str(), d.version.as_str()), (n, v), "{s}");
    }
    // A package with an unreadable dependency is refused once a closure needs it; the others still resolve.
    let i = idx(vec![("one", vec![pkg("bad", "1-1", &[("depends", &[">="])]), pkg("app", "1-1", &[("depends", &["bad"])]), pkg("ok", "1-1", &[])])]);
    let u = Universe::new(&i, Level::V3);
    let p = Closure::resolve("app", &u, &Prefs::builtin(), &[]);
    assert!(p.refused.iter().any(|(n, r)| n == "bad" && r.contains("unlesbar")), "{:?}", p.refused);
    assert!(Closure::resolve("ok", &u, &Prefs::builtin(), &[]).ok());
    // The program's own unreadable field refuses it too (it is not dropped silently).
    let i = idx(vec![("one", vec![pkg("top", "1-1", &[("depends", &["ok", ">="])]), pkg("ctl", "1-1", &[("depends", &["o\u{1}k"])]), pkg("ok", "1-1", &[])])]);
    let u = Universe::new(&i, Level::V3);
    for n in ["top", "ctl"] {
        for p in [Closure::resolve(n, &u, &Prefs::builtin(), &[]), Closure::pacman(n, &u)] {
            assert!(!p.ok() && p.refused.iter().any(|(m, r)| m == n && r.contains("unlesbares Feld")), "{n}: {:?}", p.refused);
        }
    }
    // Sizes near 2^64 from a lying database: refused, no overflow panic, no wrapped sum.
    let mut big = pkg("big", "1-1", &[("depends", &["big2"])]);
    big.csize = u64::MAX;
    let mut big2 = pkg("big2", "1-1", &[]);
    big2.isize = u64::MAX;
    big2.csize = u64::MAX;
    let i = idx(vec![("one", vec![big, big2])]);
    let u = Universe::new(&i, Level::V3);
    let p = Closure::resolve("big", &u, &Prefs::builtin(), &[]);
    assert!(!p.ok() && p.refused.iter().any(|(_, r)| r.contains("unplausible Größenangabe")), "{:?}", p.refused);
    // Huge counts: 100000 dependencies, all met by one package with 100000 provides.
    let n = 100_000;
    let ds: Vec<String> = (0..n).map(|k| format!("so{k}.so=1-64")).collect();
    let dr: Vec<&str> = ds.iter().map(String::as_str).collect();
    let i = idx(vec![("one", vec![pkg("many", "1-1", &[("depends", &dr)]), pkg("lib", "1-1", &[("provides", &dr)])])]);
    let u = Universe::new(&i, Level::V3);
    let p = Closure::resolve("many", &u, &Prefs::builtin(), &[]);
    assert!(p.ok(), "{:?}", &p.refused[..p.refused.len().min(3)]);
    assert_eq!(names(&p), ["lib", "many"]);
    // A group is no program: refused naming it, not "in keiner Quelle".
    let i = idx(vec![("one", vec![pkg("g1", "1-1", &[("groups", &["desk"])]), pkg("g2", "1-1", &[("groups", &["desk"])])])]);
    let u = Universe::new(&i, Level::V3);
    let p = Closure::resolve("desk", &u, &Prefs::builtin(), &[]);
    assert!(!p.ok() && p.refused[0].1.contains("Paketgruppe") && p.refused[0].1.contains("g1, g2"), "{:?}", p.refused);
    for w in ["", "../x", "a b", "a/b/c", &"x".repeat(5000)] {
        let p = Closure::resolve(w, &u, &Prefs::builtin(), &[]);
        assert!(!p.ok() && !p.refused.is_empty(), "{w:?}");
    }
}

#[test]
fn tables_and_file_rules() {
    let t = Prefs::builtin();
    assert_eq!(t.rules.len(), 6);
    for d in ["vulkan-driver", "opengl-driver", "ttf-font", "java-runtime", "libgl", "sh"] {
        assert!(t.rule(d).is_some(), "{d}");
    }
    assert!(Prefs::parse("x vulkan-radeon").is_err() && Prefs::parse("x ->").is_err());
    let rules = policy::system_rules();
    assert!(rules.len() >= 20);
    assert!(policy::system(&rules, "systemd", true).is_some() && policy::system(&rules, "systemd", false).is_none());
    assert!(policy::system(&rules, "systemd-libs", true).is_none() && policy::system(&rules, "linux-api-headers", false).is_none());
    assert!(policy::system(&rules, "nvidia-dkms", false).is_some() && policy::system(&rules, "amd-ucode", false).is_some());
    assert!(policy::glob("jre*-openjdk", "jre-openjdk") && policy::glob("jre*-openjdk", "jre21-openjdk") && !policy::glob("jre*-openjdk", "jdk-openjdk"));
    let desk = |t: &'static str| move |_: &str| Some(t.to_string());
    assert!(policy::is_program(["./usr/bin/jq", "usr/lib/libjq.so"], desk("")));
    assert!(!policy::is_program(["usr/lib/libjq.so", "usr/bin/"], desk("")));
    assert!(policy::is_program(["usr/share/applications/a.desktop"], desk("[Desktop Entry]\nName=A\n")));
    assert!(!policy::is_program(["usr/share/applications/a.desktop"], desk("[Desktop Entry]\nNoDisplay=true\n")));
    assert!(policy::is_program(["usr/share/applications/a.desktop"], desk("[Desktop Entry]\n[Desktop Action x]\nNoDisplay=true\n")));
    assert!(policy::system_by_files(["usr/lib/modules/7.1/x.ko"]).is_some());
    assert!(policy::system_by_files(["boot/vmlinuz"]).is_some());
    assert!(policy::system_by_files(["usr/lib/systemd/system/a.service", "usr/share/licenses/a/LICENSE"]).is_some());
    assert!(policy::system_by_files(["usr/lib/systemd/system/a.service", "usr/bin/a"]).is_none());
}

#[test]
fn plan_install_prints_the_page() {
    let root = tmp("resolve-cli");
    let sync = root.join("data/bitspkg/sync");
    std::fs::create_dir_all(&sync).unwrap();
    for r in REPOS {
        std::fs::copy(host().dir.join("db/sync").join(format!("{r}.db")), sync.join(format!("{r}.db"))).unwrap();
    }
    let (code, out, err) = common::run(&root, &["--settings", "none", "plan", "install", "jq"], None);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.starts_with("Programm: jq ") && out.contains("Pakete (in Installationsreihenfolge):") && out.contains("Download"), "{out}");
    assert!(err.contains("event=pkg.resolve.done"), "{err}");
    print!("{out}");
    let (code, out, err) = common::run(&root, &["--settings", "none", "plan", "install", "linux"], None);
    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains("abgelehnt: linux: Das ist ein Systemteil"), "{out}");
    assert!(err.contains("event=pkg.resolve.refused"), "{err}");
}
