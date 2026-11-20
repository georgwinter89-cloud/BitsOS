//! PK-6a host tests of `refresh` without a network: a stand-in for /bin/fetch serves mirror folders, so mirror
//! fallback, the older-database refusal, the signature rules (with a test double for PK-6b's verifier), the index
//! and the leftover rule (B2: only `data/bitspkg/sync` is written) are checked. The real fetch over HTTP runs in
//! the QEMU scenario host/tests/pk/70-refresh-fixture.sh. Also the `pkg.*` settings keys against settingsd.

mod common;

use bitspkg::repo::dbsig::DbVerifier;
use bitspkg::repo::download::Fetcher;
use bitspkg::repo::refresh::{self, Outcome, Refresh};
use bitspkg::repo::sources::{self, Config, Level, Sig, Source};
use bitspkg::repo::{search, unpack};
use bitspkg::settings::Settings;
use common::{sock_dir, tmp};
use settingsd::json::Json;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn header(name: &str, size: u64) -> [u8; 512] {
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    h[100..107].copy_from_slice(b"0000644");
    h[124..135].copy_from_slice(format!("{size:011o}").as_bytes());
    h[136..147].copy_from_slice(b"14000000000");
    h[156] = b'0';
    h[257..263].copy_from_slice(b"ustar\0");
    h[148..156].copy_from_slice(b"        ");
    let sum: u64 = h.iter().map(|&b| b as u64).sum();
    h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
    h
}

/// A gzip database of `pkgs` (name, version, builddate, makedepends).
fn db(pkgs: &[(&str, &str, i64, &str)]) -> Vec<u8> {
    let mut t = Vec::new();
    for (n, v, date, make) in pkgs {
        let d = format!(
            "%FILENAME%\n{n}-{v}-x86_64.pkg.tar.zst\n\n%NAME%\n{n}\n\n%VERSION%\n{v}\n\n%DESC%\n{n} does things\n\n%CSIZE%\n10\n\n%ISIZE%\n20\n\n%SHA256SUM%\n{}\n\n%BUILDDATE%\n{date}\n\n%MAKEDEPENDS%\n{make}\n\n",
            "cd".repeat(32)
        );
        t.extend_from_slice(&header(&format!("{n}-{v}/desc"), d.len() as u64));
        t.extend_from_slice(d.as_bytes());
        t.resize(t.len().div_ceil(512) * 512, 0);
    }
    t.resize(t.len() + 1024, 0);
    let mut v = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3];
    v.extend(miniz_oxide::deflate::compress_to_vec(&t, 1));
    v.extend(unpack::crc32(&t).to_le_bytes());
    v.extend((t.len() as u32).to_le_bytes());
    v
}

fn put(p: &Path, b: &[u8]) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, b).unwrap();
}

/// A stand-in for fetch: `http://fix/<path>` is `<mirrors>/<path>`, a missing file is HTTP 404 (exit 7).
fn fake_fetch(d: &Path, mirrors: &Path) -> Fetcher {
    let exe = d.join("fetch");
    let script = format!(
        "#!/bin/bash\nout=\"\"; url=\"\"\nwhile [ $# -gt 0 ]; do case \"$1\" in -o) out=\"$2\"; shift 2;; --) url=\"$2\"; shift 2;; -q) shift;; *) shift 2;; esac; done\nsrc=\"{}/${{url#http://fix/}}\"\nif [ -f \"$src\" ]; then cp \"$src\" \"$out\"; else echo \"fetch: HTTP 404 Not Found\" >&2; exit 7; fi\n",
        mirrors.display()
    );
    std::fs::write(&exe, script).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    Fetcher { exe, drop_to: None, timeout: 5, max_time: 20 }
}

/// PK-6b's verifier stand-in: a signature is good when it is the bytes "GOOD".
struct Double;
impl DbVerifier for Double {
    fn verify(&self, keyring: &str, _data: &Path, sig: &[u8]) -> Result<String, String> {
        if sig == b"GOOD" { Ok(format!("test key of {keyring}")) } else { Err("Signatur passt nicht (Testdoppel)".into()) }
    }
}

fn src(name: &str, servers: &[&str], sig: Sig) -> Source {
    Source { name: name.into(), servers: servers.iter().map(|s| s.to_string()).collect(), sig_db: sig, sig_pkg: Sig::Required, keyring: "archlinux".into() }
}

fn cfg(sources: Vec<Source>) -> Config {
    Config { level: Level::V3, sources, origin: "test".into(), space_max_gb: None, updates: Default::default() }
}

fn outcome(root: &Path, f: &Fetcher, c: &Config) -> Vec<Outcome> {
    let r = Refresh { root: root.to_path_buf(), fetcher: f, verifier: &Double, now: 1_760_100_000 };
    r.run(c).unwrap().into_iter().map(|r| r.outcome).collect()
}

fn files_below(p: &Path) -> Vec<String> {
    let mut v = Vec::new();
    let mut stack: Vec<PathBuf> = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let q = e.path();
            v.push(q.strip_prefix(p).unwrap().display().to_string());
            if e.file_type().unwrap().is_dir() {
                stack.push(q);
            }
        }
    }
    v.sort();
    v
}

#[test]
fn refresh_falls_back_refuses_older_and_checks_signatures() {
    let d = tmp("repo-refresh");
    let (m, root) = (d.join("mirrors"), d.join("root"));
    std::fs::create_dir_all(&root).unwrap();
    let f = fake_fetch(&d, &m);
    put(&m.join("m1/fix/fix.db"), b"\x1f\x8bnot really gzip");
    put(&m.join("m2/fix/fix.db"), &db(&[("jq", "1.8.1-1", 1_760_000_000, "autoconf"), ("ripgrep", "15.1.0-1", 1_759_000_000, "cargo"), ("ripgrep-all", "0.10-1", 1_758_000_000, "")]));
    put(&m.join("m3/fix/fix.db"), &db(&[("jq", "1.7-1", 1_750_000_000, "")]));
    put(&m.join("m4/fix/fix.db"), &db(&[("jq", "1.8.2-1", 1_761_000_000, "")]));

    let two = cfg(vec![src("fix", &["http://fix/m1/$repo", "http://fix/m2/$repo"], Sig::Optional)]);
    let o = outcome(&root, &f, &two);
    assert!(matches!(&o[0], Outcome::Updated { pkgs: 3, mirror, .. } if mirror.contains("/m2/")), "{o:?}");
    let sync = refresh::sync_dir(&root);
    assert!(sync.join("fix.idx").is_file() && sync.join("fix.fresh").is_file());
    let report = std::fs::read_to_string(sync.join("last-refresh.txt")).unwrap();
    assert!(report.contains("http://fix/m1/fix") && report.contains("aktualisiert"), "{report}");

    // Search: exact name first, then Rust first among the prefix matches.
    let (idx, missing) = search::indexes(&root, &two);
    assert!(missing.is_empty());
    let names: Vec<String> = search::search(&idx, "ripgrep").into_iter().map(|h| h.pkg.name).collect();
    assert_eq!(names, ["ripgrep", "ripgrep-all"]);
    let names: Vec<String> = search::search(&idx, "does").into_iter().map(|h| h.pkg.name).collect();
    assert_eq!(names[0], "ripgrep", "Rust first among equal matches: {names:?}");
    assert_eq!(search::find(&idx, "fix/jq").map(|(_, p)| p.version.clone()), Some("1.8.1-1".into()));

    // The same database again: unchanged. An older one: refused, the current one stays.
    assert!(matches!(&outcome(&root, &f, &two)[0], Outcome::Unchanged { .. }));
    let older = cfg(vec![src("fix", &["http://fix/m3/$repo"], Sig::Optional)]);
    match &outcome(&root, &f, &older)[0] {
        Outcome::Refused(why) => assert!(why.contains("älter"), "{why}"),
        o => panic!("older database taken: {o:?}"),
    }
    assert_eq!(refresh::load(&sync, "fix").unwrap().pkgs["jq"].version, "1.8.1-1");
    let newer = cfg(vec![src("fix", &["http://fix/m3/$repo", "http://fix/m4/$repo"], Sig::Optional)]);
    assert!(matches!(&outcome(&root, &f, &newer)[0], Outcome::Updated { pkgs: 1, .. }));

    // An index made for another database (a cut between the renames) is rebuilt from the database.
    let ip = sync.join("fix.idx");
    let t = std::fs::read_to_string(&ip).unwrap();
    std::fs::write(&ip, t.replacen("db_sha256 ", "db_sha256 0", 1)).unwrap();
    assert_eq!(refresh::load(&sync, "fix").unwrap().pkgs["jq"].version, "1.8.2-1");

    // The freshness names the database it belongs to.
    let fr = refresh::read_fresh(&sync, "fix");
    assert_eq!(fr.db_sha256, refresh::load(&sync, "fix").unwrap().db_sha256);
    // A search over two sources lists the later same-named package, marked as hidden.
    let two_idx = vec![("a".to_string(), refresh::load(&sync, "fix").unwrap()), ("b".to_string(), refresh::load(&sync, "fix").unwrap())];
    let hits = search::search(&two_idx, "jq");
    assert_eq!(hits.iter().map(|h| (h.repo.as_str(), h.shadowed_by.as_deref())).collect::<Vec<_>>(), [("a", None), ("b", Some("a"))]);

    // A mirror that synced later may drop the newest package by a little (a revert): taken. By much: refused.
    // Then `refresh --reset` forgets the source, and an older database is taken again.
    put(&m.join("m5/fix/fix.db"), &db(&[("jq", "1.8.2-1", 1_760_900_000, "")]));
    put(&m.join("m5/lastupdate"), b"1761100000\n");
    put(&m.join("m6/fix/fix.db"), &db(&[("jq", "1.8.0-1", 1_755_000_000, "")]));
    put(&m.join("m6/lastupdate"), b"1761200000\n");
    let dropped = cfg(vec![src("fix", &["http://fix/m5/$repo"], Sig::Optional)]);
    assert!(matches!(&outcome(&root, &f, &dropped)[0], Outcome::Updated { .. }), "a small drop with a later lastupdate is taken");
    assert_eq!(refresh::read_fresh(&sync, "fix").lastupdate, 1_761_100_000);
    assert_eq!(refresh::read_fresh(&sync, "fix").builddate, 1_761_000_000, "the known newest stays (no ratchet down)");
    let far = cfg(vec![src("fix", &["http://fix/m6/$repo"], Sig::Optional)]);
    match &outcome(&root, &f, &far)[0] {
        Outcome::Refused(why) => assert!(why.contains("--reset fix"), "{why}"),
        o => panic!("a large drop was taken: {o:?}"),
    }
    refresh::reset(&root, "fix").unwrap();
    assert!(!sync.join("fix.db").exists() && !sync.join("fix.fresh").exists());
    assert!(matches!(&outcome(&root, &f, &older)[0], Outcome::Updated { pkgs: 1, .. }), "after the reset the older one is taken");
    assert!(refresh::reset(&root, "../x").is_err());

    // Signatures: required and missing, bad, good; an optional one present is checked too.
    put(&m.join("s1/sig/sig.db"), &db(&[("foot", "1.24-1", 1_760_000_000, "")]));
    put(&m.join("s2/sig/sig.db"), &db(&[("foot", "1.24-1", 1_760_000_000, "")]));
    put(&m.join("s2/sig/sig.db.sig"), b"BAD");
    put(&m.join("s3/sig/sig.db"), &db(&[("foot", "1.24-1", 1_760_000_000, "")]));
    put(&m.join("s3/sig/sig.db.sig"), b"GOOD");
    for (servers, ok, want) in [
        (&["http://fix/s1/$repo"][..], false, "fehlt"),
        (&["http://fix/s2/$repo"][..], false, "abgelehnt"),
        (&["http://fix/s1/$repo", "http://fix/s2/$repo", "http://fix/s3/$repo"][..], true, ""),
    ] {
        match &outcome(&root, &f, &cfg(vec![src("sig", servers, Sig::Required)]))[0] {
            Outcome::Updated { .. } | Outcome::Unchanged { .. } if ok => {}
            Outcome::Refused(why) if !ok => assert!(why.contains(want), "{servers:?}: {why}"),
            o => panic!("{servers:?}: {o:?}"),
        }
    }
    assert!(sync.join("sig.db.sig").is_file());
    put(&m.join("o1/opt/opt.db"), &db(&[("foot", "1.24-1", 1_760_000_000, "")]));
    put(&m.join("o1/opt/opt.db.sig"), b"BAD");
    match &outcome(&root, &f, &cfg(vec![src("opt", &["http://fix/o1/$repo"], Sig::Optional)]))[0] {
        Outcome::Refused(why) => assert!(why.contains("abgelehnt"), "{why}"),
        o => panic!("a bad optional signature was taken: {o:?}"),
    }
    // An optional signature: a good one is taken and remembered; a later database without one is then refused
    // (stricter than pacman's DatabaseOptional), until `reset` forgets the repository.
    put(&m.join("o2/opt/opt.db"), &db(&[("foot", "1.24-1", 1_760_000_000, "")]));
    put(&m.join("o2/opt/opt.db.sig"), b"GOOD");
    put(&m.join("o3/opt/opt.db"), &db(&[("foot", "1.25-1", 1_760_100_000, "")]));
    assert!(matches!(&outcome(&root, &f, &cfg(vec![src("opt", &["http://fix/o2/$repo"], Sig::Optional)]))[0], Outcome::Updated { .. }));
    match &outcome(&root, &f, &cfg(vec![src("opt", &["http://fix/o3/$repo"], Sig::Optional)]))[0] {
        Outcome::Refused(why) => assert!(why.contains("zuletzt angenommene war signiert"), "{why}"),
        o => panic!("an unsigned database after a signed one was taken: {o:?}"),
    }
    refresh::reset(&root, "opt").unwrap();
    assert!(matches!(&outcome(&root, &f, &cfg(vec![src("opt", &["http://fix/o3/$repo"], Sig::Optional)]))[0], Outcome::Updated { .. }));

    // A level this CPU cannot run is refused before anything is fetched; no servers is a reason too.
    let o = outcome(&root, &f, &cfg(vec![src("fix-v4", &["http://fix/m2/fix"], Sig::Optional), src("none", &[], Sig::Optional)]));
    assert_eq!(matches!(&o[0], Outcome::Refused(w) if w.contains("Prozessor")), !sources::cpu_can(Level::V4), "{o:?}");
    assert!(matches!(&o[1], Outcome::Refused(w) if w.contains("keine Server")), "{o:?}");

    // B2: refresh wrote only below data/bitspkg/sync, and left no download folder.
    let all = files_below(&root);
    assert!(all.iter().all(|p| p == "data" || p == "data/bitspkg" || p.starts_with("data/bitspkg/sync")), "{all:?}");
    assert!(!all.iter().any(|p| p.contains("download-") || p.ends_with(".part")), "{all:?}");
}

#[test]
fn sources_defaults_mirror_lists_and_settings_keys() {
    // D1: CachyOS v3 first (as on the build host's pacman.conf), then cachyos, core, extra; no multilib.
    let c = sources::defaults("t");
    let names: Vec<&str> = c.sources.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["cachyos-v3", "cachyos-extra-v3", "cachyos-core-v3", "cachyos", "core", "extra"]);
    assert_eq!(c.sources[0].sig_db, Sig::Required);
    assert_eq!(c.sources[4].sig_db, Sig::Optional);
    assert!(sources::expand_order("core multilib", Level::V3).is_err());
    assert!(sources::expand_order("core-testing", Level::V3).is_err());
    assert_eq!(sources::expand_order(sources::ORDER_DEFAULT, Level::Generic).unwrap(), ["cachyos", "core", "extra"]);
    // The build host's own mirror lists: $arch and $repo filled as pacman does ($arch first, so $arch_v3 works).
    let d = tmp("repo-sources");
    for f in ["mirrorlist", "cachyos-v3-mirrorlist"] {
        std::fs::copy(Path::new("/etc/pacman.d").join(f), d.join(f)).unwrap_or_else(|e| panic!("/etc/pacman.d/{f}: {e}"));
    }
    let (urls, problems) = sources::servers(&c.sources[0], &d);
    assert!(problems.is_empty() && !urls.is_empty(), "{problems:?}");
    assert!(urls.iter().all(|u| u.contains("/x86_64_v3/cachyos-v3")), "{urls:?}");
    let (urls, _) = sources::servers(&c.sources[5], &d);
    assert!(urls.iter().all(|u| u.contains("/extra/") && !u.contains('$')), "{urls:?}");
    let (_, problems) = sources::servers(&src("x", &["include:../etc/passwd", "ftp://no/$repo", "include:nolist"], Sig::Optional), &d);
    assert_eq!(problems.len(), 3, "{problems:?}");

    // settingsd: bitspkg registers pkg.cpu_level, pkg.sources.order and each source's keys, and reads them back.
    let run = sock_dir();
    let sock = run.join("s.sock");
    bitspkg::clean::start_settingsd(&d.join("settings"), &sock).unwrap();
    let s = Settings { socket: sock };
    let c = sources::from_settings(&s).unwrap();
    assert_eq!(c.sources.len(), 6);
    assert_eq!(c.origin, "settingsd");
    for (k, v) in [("pkg.sources.order", "core fixture"), ("pkg.sources.fixture.servers", "http://10.0.2.2:1/$repo")] {
        let set = Json::obj().with("op", "set").with("key", k).with("value", v).with("source", "test");
        if k.contains("fixture") {
            sources::from_settings(&s).unwrap();
        }
        s.request(&set).unwrap().unwrap();
    }
    let c = sources::from_settings(&s).unwrap();
    assert_eq!(c.sources.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["core", "fixture"]);
    assert_eq!(c.sources[1].servers, ["http://10.0.2.2:1/$repo"]);
    assert_eq!(c.sources[1].sig_db, Sig::Required, "an unknown source's database needs a signature");
}
