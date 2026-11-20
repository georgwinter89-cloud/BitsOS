//! PK-6a host tests: the tar reader, gzip/zstd, the sync database parser against every database of the build host
//! (package counts equal to `pacman -Sl` on a scratch copy, as uid 1000), pacman's version order against
//! /usr/bin/vercmp, hostile databases, the index. Nothing here touches the host's pacman state: databases are
//! copied into the test's own folder first.

mod common;

use bitspkg::repo::syncdb::{self, Db};
use bitspkg::repo::tar::{Kind, Tar};
use bitspkg::repo::{index, unpack, vercmp};
use common::tmp;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---- building archives ----

pub fn header(name: &str, ty: u8, size: u64, link: &str) -> [u8; 512] {
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    h[100..107].copy_from_slice(b"0000644");
    h[108..115].copy_from_slice(b"0000000");
    h[116..123].copy_from_slice(b"0000000");
    h[124..135].copy_from_slice(format!("{size:011o}").as_bytes());
    h[136..147].copy_from_slice(b"14000000000");
    h[156] = ty;
    h[157..157 + link.len()].copy_from_slice(link.as_bytes());
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[148..156].copy_from_slice(b"        ");
    let sum: u64 = h.iter().map(|&b| b as u64).sum();
    h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
    h
}

pub fn tarball(entries: &[(&str, u8, &[u8], &str)]) -> Vec<u8> {
    let mut v = Vec::new();
    for (name, ty, data, link) in entries {
        v.extend_from_slice(&header(name, *ty, data.len() as u64, link));
        v.extend_from_slice(data);
        v.resize(v.len().div_ceil(512) * 512, 0);
    }
    v.resize(v.len() + 1024, 0);
    v
}

pub fn gz(data: &[u8]) -> Vec<u8> {
    let mut v = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3];
    v.extend(miniz_oxide::deflate::compress_to_vec(data, 1));
    v.extend(unpack::crc32(data).to_le_bytes());
    v.extend((data.len() as u32).to_le_bytes());
    v
}

pub fn desc(name: &str, ver: &str, extra: &str) -> String {
    format!(
        "%FILENAME%\n{name}-{ver}-x86_64.pkg.tar.zst\n\n%NAME%\n{name}\n\n%VERSION%\n{ver}\n\n%DESC%\nThe {name} program\n\n%CSIZE%\n100\n\n%ISIZE%\n300\n\n%SHA256SUM%\n{}\n\n%BUILDDATE%\n1760000000\n\n{extra}",
        "ab".repeat(32)
    )
}

fn parse_bytes(b: Vec<u8>) -> Result<Db, String> {
    syncdb::parse(std::io::Cursor::new(b), "test")
}

// ---- tar ----

#[test]
fn tar_reads_every_kind_it_allows() {
    let long = format!("{}/{}", "d".repeat(80), "f".repeat(80));
    let mut pax_rec = format!(" path={long}\n");
    pax_rec = format!("{}{pax_rec}", pax_rec.len() + 3);
    let lname = format!("{}/gnu-long", "g".repeat(120));
    let mut lbytes = lname.clone().into_bytes();
    lbytes.push(0);
    let t = tarball(&[
        ("./top/", b'5', b"", ""),
        ("top/file", b'0', b"hello", ""),
        ("top/old", 0, b"x", ""),
        ("top/link", b'2', b"", "../top/file"),
        ("top/hard", b'1', b"", "top/file"),
        ("ignored", b'x', pax_rec.as_bytes(), ""),
        ("short", b'0', b"pax", ""),
        ("././@LongLink", b'L', &lbytes, ""),
        ("cut", b'0', b"gnu", ""),
    ]);
    let mut r = Tar::new(&t[..]);
    let mut got = Vec::new();
    while let Some(e) = r.next_entry().unwrap() {
        let mut d = vec![0u8; 16];
        let n = r.read_data(&mut d).unwrap();
        got.push((e.path, e.kind, String::from_utf8_lossy(&d[..n]).into_owned()));
    }
    assert_eq!(
        got,
        vec![
            ("top".into(), Kind::Dir, String::new()),
            ("top/file".into(), Kind::File, "hello".into()),
            ("top/old".into(), Kind::File, "x".into()),
            ("top/link".into(), Kind::Symlink("../top/file".into()), String::new()),
            ("top/hard".into(), Kind::HardLink("top/file".into()), String::new()),
            (long, Kind::File, "pax".into()),
            (lname, Kind::File, "gnu".into()),
        ]
    );
}

#[test]
fn tar_refuses_what_it_must() {
    for (entries, want) in [
        (vec![("/etc/passwd", b'0', &b"x"[..], "")], "absolute"),
        (vec![("a/../../b", b'0', &b"x"[..], "")], "leaves the tree"),
        (vec![("dev", b'3', &b""[..], "")], "device"),
        (vec![("blk", b'4', &b""[..], "")], "device"),
        (vec![("fifo", b'6', &b""[..], "")], "FIFO"),
        (vec![("sparse", b'S', &b""[..], "")], "sparse"),
        (vec![("hard", b'1', &b""[..], "/etc/shadow")], "absolute"),
    ] {
        let t = tarball(&entries);
        let e = Tar::new(&t[..]).next_entry().unwrap_err();
        assert!(e.contains(want), "{entries:?}: {e}");
    }
    let mut t = tarball(&[("a", b'0', b"data", "")]);
    t[0] ^= 1;
    assert!(Tar::new(&t[..]).next_entry().unwrap_err().contains("checksum"));
    let t = tarball(&[("a", b'0', &[7u8; 2000], "")]);
    let mut r = Tar::new(&t[..1000]);
    r.next_entry().unwrap();
    let mut buf = vec![0u8; 4096];
    assert!(r.read_data(&mut buf).unwrap_err().contains("truncated"));
    // A pax size near u64::MAX is refused before any arithmetic on it (no overflow outside catch_unwind).
    let t = tarball(&[("pax", b'x', b"29 size=18446744073709551000\n", ""), ("f", b'0', b"", "")]);
    let e = Tar::new(&t[..]).next_entry().unwrap_err();
    assert!(e.contains("at most"), "{e}");
}

// ---- compression ----

#[test]
fn gzip_members_zstd_frames_and_damage() {
    let mut two = gz(b"hello ");
    two.extend(gz(b"world"));
    two.extend([0u8; 7]);
    let mut s = String::new();
    unpack::open(std::io::Cursor::new(two)).unwrap().read_to_string(&mut s).unwrap();
    assert_eq!(s, "hello world");
    let mut bad = gz(b"hello world, a longer text to hold a crc");
    let n = bad.len();
    bad[n - 6] ^= 0xff;
    let mut v = Vec::new();
    let e = unpack::open(std::io::Cursor::new(bad)).unwrap().read_to_end(&mut v).unwrap_err();
    assert!(e.to_string().contains("Prüfsumme"), "{e}");
    let mut z = ruzstd::encoding::compress_to_vec(&b"zstd one "[..], ruzstd::encoding::CompressionLevel::Fastest);
    z.extend(ruzstd::encoding::compress_to_vec(&b"and two"[..], ruzstd::encoding::CompressionLevel::Fastest));
    let mut s = String::new();
    unpack::open(std::io::Cursor::new(z.clone())).unwrap().read_to_string(&mut s).unwrap();
    assert_eq!(s, "zstd one and two");
    z.truncate(z.len() - 3);
    let mut v = Vec::new();
    assert!(unpack::open(std::io::Cursor::new(z)).unwrap().read_to_end(&mut v).is_err());

    // zstd(1) writes the frame checksum (XXH64): a frame whose content was changed but still decodes is refused.
    let data: Vec<u8> = (0..5000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    let z = zstd_cli(&data);
    let mut v = Vec::new();
    unpack::open(std::io::Cursor::new(z.clone())).unwrap().read_to_end(&mut v).unwrap();
    assert_eq!(v, data);
    let at = z.windows(16).position(|w| w == &data[100..116]).expect("an uncompressed block in the frame") + 3;
    let mut bad = z;
    bad[at] ^= 0x55;
    let mut v = Vec::new();
    let e = unpack::open(std::io::Cursor::new(bad)).unwrap().read_to_end(&mut v).unwrap_err();
    assert!(e.to_string().contains("Prüfsumme"), "{e}");
}

fn zstd_cli(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut c = Command::new("zstd").args(["-q", "-c", "--check", "-1"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().expect("zstd(1) on the build host");
    c.stdin.take().unwrap().write_all(data).unwrap();
    let o = c.wait_with_output().unwrap();
    assert!(o.status.success());
    o.stdout
}

#[test]
fn xxh64_equals_xxhsum() {
    assert_eq!(xxh(b""), 0xEF46_DB37_51D8_E999);
    let d = tmp("repo-xxh");
    let data: Vec<u8> = (0..3000u32).map(|i| (i * 7 + i / 5) as u8).collect();
    for n in [0usize, 1, 3, 4, 7, 8, 9, 31, 32, 33, 63, 64, 100, 1000, 3000] {
        let f = d.join(format!("x{n}"));
        std::fs::write(&f, &data[..n]).unwrap();
        let o = Command::new("xxhsum").arg("-H64").arg(&f).output().expect("xxhsum on the build host");
        let t = String::from_utf8_lossy(&o.stdout);
        let want = u64::from_str_radix(t.split_whitespace().next().unwrap().trim_start_matches("XXH64_"), 16).unwrap();
        assert_eq!(xxh(&data[..n]), want, "length {n}");
        // Fed in uneven pieces, the same.
        let mut h = bitspkg::repo::xxh64::Xxh64::default();
        for c in data[..n].chunks(13) {
            h.update(c);
        }
        assert_eq!(h.finish(), want, "length {n} in pieces");
    }
}

fn xxh(b: &[u8]) -> u64 {
    let mut h = bitspkg::repo::xxh64::Xxh64::default();
    h.update(b);
    h.finish()
}

// ---- the build host's databases ----

fn host_dbs(d: &Path) -> Vec<(String, PathBuf)> {
    let src = Path::new("/var/lib/pacman/sync");
    let mut v = Vec::new();
    for e in std::fs::read_dir(src).expect("/var/lib/pacman/sync must exist on the build host (CachyOS)").flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if let Some(repo) = n.strip_suffix(".db") {
            let to = d.join("sync").join(&n);
            std::fs::create_dir_all(to.parent().unwrap()).unwrap();
            std::fs::copy(e.path(), &to).unwrap();
            v.push((repo.to_string(), to));
        }
    }
    v.sort();
    assert!(v.len() >= 2, "the build host has fewer than two sync databases: {v:?}");
    v
}

fn pacman_count(scratch: &Path, repos: &[(String, PathBuf)], repo: &str) -> usize {
    let conf = scratch.join("pacman.conf");
    let mut c = String::from("[options]\nSigLevel = Never\n");
    for (r, _) in repos {
        c.push_str(&format!("[{r}]\nServer = http://127.0.0.1:1/$repo\n"));
    }
    std::fs::write(&conf, c).unwrap();
    let out = Command::new("/usr/bin/pacman").arg("--config").arg(&conf).arg("--dbpath").arg(scratch).args(["-Sl", repo]).output().expect("/usr/bin/pacman");
    assert!(out.status.success(), "pacman -Sl {repo}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).lines().count()
}

#[test]
fn every_host_database_parses_with_pacmans_count() {
    assert_ne!(usys::getuid().unwrap(), 0, "runs as uid 1000 (pacman on a scratch copy, never as root)");
    let d = tmp("repo-host-dbs");
    let dbs = host_dbs(&d);
    let mut versions = 0;
    for (repo, p) in &dbs {
        let db = syncdb::parse(std::fs::File::open(p).unwrap(), repo).unwrap_or_else(|e| panic!("{repo}: {e}"));
        let want = pacman_count(&d, &dbs, repo);
        println!("{repo}: {} packages (pacman -Sl: {want}), newest {}", db.pkgs.len(), db.newest());
        assert_eq!(db.pkgs.len(), want, "{repo}");
        assert!(db.pkgs.values().all(|p| p.sha256.len() == 64 && !p.filename.is_empty()), "{repo}: sha256 and filename");
        // The index carries every field through.
        let text = index::render(&db, "00");
        let back = index::parse(&text).unwrap();
        assert_eq!(back.pkgs, db.pkgs, "{repo}: index round trip");
        versions += db.pkgs.len();
        // Truncated at a third: refused with a reason, never a panic.
        let bytes = std::fs::read(p).unwrap();
        let e = parse_bytes(bytes[..bytes.len() / 3].to_vec()).unwrap_err();
        println!("{repo} truncated: {e}");
    }
    assert!(versions > 1000);
}

// ---- vercmp ----

fn host_vercmp(pairs: &[(String, String)]) -> Vec<i32> {
    let chunks: Vec<&[(String, String)]> = pairs.chunks(pairs.len().div_ceil(8).max(1)).collect();
    std::thread::scope(|s| {
        let hs: Vec<_> = chunks
            .iter()
            .map(|c| {
                s.spawn(move || {
                    c.iter()
                        .map(|(a, b)| {
                            let o = Command::new("/usr/bin/vercmp").arg(a).arg(b).output().expect("/usr/bin/vercmp");
                            String::from_utf8_lossy(&o.stdout).trim().parse::<i32>().expect("vercmp output")
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
    })
}

#[test]
fn vercmp_equals_pacmans_on_20000_pairs_and_the_hand_cases() {
    let src = std::env::var("BITSPKG_PACMAN_SRC").unwrap_or_else(|_| "/var/tmp/claude-v2/ref/pacman-7.1".into());
    let hand = std::fs::read_to_string(format!("{src}/test/util/vercmptest.sh")).expect("pacman 7.1 source (Prep step, or BITSPKG_PACMAN_SRC)");
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut want = Vec::new();
    for l in hand.lines().filter_map(|l| l.strip_prefix("tap_runtest ")) {
        let f: Vec<&str> = l.split_whitespace().collect();
        let e: i32 = f[2].parse().unwrap();
        pairs.push((f[0].into(), f[1].into()));
        want.push(Some(e));
        pairs.push((f[1].into(), f[0].into()));
        want.push(Some(-e));
    }
    assert!(pairs.len() >= 90, "hand cases: {}", pairs.len());
    let d = tmp("repo-vercmp");
    let mut all: Vec<String> = Vec::new();
    let mut by_name: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for (repo, p) in host_dbs(&d) {
        for pk in syncdb::parse(std::fs::File::open(&p).unwrap(), &repo).unwrap().pkgs.into_values() {
            all.push(pk.version.clone());
            by_name.entry(pk.name).or_default().push(pk.version);
        }
    }
    all.sort();
    all.dedup();
    // The same name in several repositories (the comparisons that matter), then deterministic far pairs.
    for vs in by_name.values().filter(|v| v.len() > 1) {
        for w in vs.windows(2) {
            pairs.push((w[0].clone(), w[1].clone()));
            want.push(None);
        }
    }
    let n = all.len();
    let mut i = 0usize;
    while pairs.len() < 20_000 + 94 {
        let (a, b) = (i % n, (i * 7919 + 13) % n);
        pairs.push((all[a].clone(), all[b].clone()));
        want.push(None);
        i += 1;
    }
    let host = host_vercmp(&pairs);
    let mut bad = Vec::new();
    for (k, ((a, b), h)) in pairs.iter().zip(&host).enumerate() {
        let ours = vercmp::vercmp(a, b);
        if ours != *h || want[k].is_some_and(|w| w != ours) {
            bad.push(format!("{a} {b}: ours {ours}, vercmp {h}, want {:?}", want[k]));
        }
    }
    println!("vercmp: {} pairs compared with /usr/bin/vercmp", pairs.len());
    assert!(bad.is_empty(), "{} differences:\n{}", bad.len(), bad[..bad.len().min(20)].join("\n"));
}

// ---- hostile databases ----

#[test]
fn hostile_databases_are_refused_with_a_reason() {
    let ok = desc("jq", "1.8.1-1", "%DEPENDS%\noniguruma\n\n");
    let db = parse_bytes(gz(&tarball(&[("jq-1.8.1-1/", b'5', b"", ""), ("jq-1.8.1-1/desc", b'0', ok.as_bytes(), "")]))).unwrap();
    assert_eq!(db.pkgs["jq"].depends, vec!["oniguruma"]);
    let other = desc("jq", "1.8.1-1", "%DEPENDS%\nevil\n\n");
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("doubled block", gz(&tarball(&[("jq-1.8.1-1/desc", b'0', ok.as_bytes(), ""), ("jq-1.8.1-1/desc", b'0', other.as_bytes(), "")])), "doppelt"),
        ("two versions", gz(&tarball(&[("jq-1.8.1-1/desc", b'0', ok.as_bytes(), ""), ("jq-1.9-1/desc", b'0', ok.as_bytes(), "")])), "zweimal"),
        ("symlink out", gz(&tarball(&[("jq-1.8.1-1/desc", b'2', b"", "../../../etc/passwd")])), "Verweis"),
        ("hard link", gz(&tarball(&[("jq-1.8.1-1/desc", b'1', b"", "etc/passwd")])), "Verweis"),
        ("dot-dot name", gz(&tarball(&[("../jq-1.8.1-1/desc", b'0', ok.as_bytes(), "")])), "leaves the tree"),
        ("long field", gz(&tarball(&[("jq-1.8.1-1/desc", b'0', format!("%DESC%\n{}\n", "a".repeat(100 << 10)).as_bytes(), "")])), "höchstens"),
        ("bad name", gz(&tarball(&[("-jq-1.8.1-1/desc", b'0', ok.as_bytes(), "")])), "Paketname"),
        ("path in filename", gz(&tarball(&[("jq-1.8.1-1/desc", b'0', ok.replace("jq-1.8.1-1-x86_64", "../x").as_bytes(), "")])), "Dateiname"),
        ("not a tar", gz(b"just text, no archive at all, long enough for one header? no"), "truncated"),
        ("xz", b"\xfd7zXZ\0rest".to_vec(), "xz"),
        ("bad sha", gz(&tarball(&[("jq-1.8.1-1/desc", b'0', ok.replace(&"ab".repeat(32), "zz").as_bytes(), "")])), "SHA-256"),
    ];
    for (what, bytes, want) in cases {
        let e = parse_bytes(bytes).unwrap_err();
        assert!(e.contains(want), "{what}: {e}");
    }
}

/// 300 MiB of zeros as one entry, packed as 300 gzip members of 1 MiB (each a few hundred bytes): refused at the
/// 256 MiB bound, without holding it in memory.
#[test]
fn tar_bomb_and_huge_field_stop_at_their_bounds() {
    let mib = vec![0u8; 1 << 20];
    let member = gz(&mib);
    let mut head = header("jq-1.8.1-1/junk", b'0', 300 << 20, "").to_vec();
    head.extend_from_slice(&mib[..(1 << 20) - 512]);
    let mut bomb = gz(&head);
    for _ in 1..300 {
        bomb.extend_from_slice(&member);
    }
    println!("bomb: {} bytes packed", bomb.len());
    let e = parse_bytes(bomb).unwrap_err();
    assert!(e.contains("256 MiB"), "{e}");
    // A 300 MiB "field": the desc entry is refused by its size before any of it is read.
    let mut huge = header("jq-1.8.1-1/desc", b'0', 300 << 20, "").to_vec();
    huge.extend_from_slice(b"%DESC%\naaaa");
    huge.resize(64 << 10, b'a');
    let e = parse_bytes(gz(&huge)).unwrap_err();
    assert!(e.contains("Beschreibung"), "{e}");
}
