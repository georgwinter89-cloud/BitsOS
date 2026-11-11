//! `xtask release-assets --version V`: everything a person without the developer's machine needs, as GitHub release
//! assets in `target/release-assets/`: the system (bitsboot, kernel, initramfs, firmware with its licences,
//! `\BITSOS2\bin\`), one archive per tree BitsOS may pass on (Mesa, Xwayland, LibreOffice, vkQuake with LibreQuake's
//! free data), the open-source part of the Brave and Steam trees with `VENDOR.txt` (their makers' files the
//! installer fetches itself), `manifest.json`, and `bitsos-install` built with that manifest's SHA-256. Every archive
//! is tar + zstd in the layout `cargo xtask image` puts on a stick, below GitHub's 2 GiB per asset.
mod vendor;

use crate::image::{self, BuildOpts};
use crate::util::*;
use bitsos_install::manifest::{Asset, Kind, MAX_ASSET, Manifest};
use bitsos_install::{release as rel, sha, stick, tar};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Trees passed on as they are, with the trees they run on.
const TREES: &[(&str, &[&str])] = &[("mesa", &[]), ("xwayland", &["mesa"]), ("libreoffice", &[]), ("vkquake", &["mesa"])];
/// Trees whose makers' files are fetched by the installer.
const RUNTIMES: &[(&str, &[&str])] = &[("brave", &[]), ("steam", &["mesa", "xwayland"])];

pub const HELP: &str = "usage: cargo xtask release-assets --version V [--out DIR] [--trees a,b|none] [--zstd-level N] [--no-installer]
  Builds the lean release image (V2_LEAN, --release) and packs the release assets into DIR
  (default target/release-assets): bitsos-system-V.tar.zst, bitsos-<tree>-V.tar.zst for mesa, xwayland,
  libreoffice, vkquake, bitsos-<tree>-runtime-V.tar.zst for brave and steam, manifest.json, SHA256SUMS, and
  bitsos-install for Linux (static, musl) and Windows (when its target and mingw are there) with the manifest's
  hash built in. --trees none packs the system only.";

/// One file of an archive: its path in the archive and where its bytes are.
struct Member {
    path: String,
    from: Src,
}

enum Src {
    File(PathBuf),
    Bytes(Vec<u8>),
}

impl Member {
    fn file(path: impl Into<String>, from: &Path) -> Member {
        Member { path: path.into(), from: Src::File(from.to_path_buf()) }
    }
    fn bytes(path: impl Into<String>, b: Vec<u8>) -> Member {
        Member { path: path.into(), from: Src::Bytes(b) }
    }
}

/// Packs `members` into `out` (tar piped through `zstd`); returns (unpacked bytes, files).
fn pack(out: &Path, members: &[Member], level: u32) -> Result<(u64, u64), String> {
    let tmp = out.with_extension("zst.part");
    let mut z = Command::new("zstd")
        .args(["-q", "-f", "-T6", &format!("-{level}"), "-o"])
        .arg(&tmp)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start zstd: {e}"))?;
    let (mut bytes, mut files) = (0u64, 0u64);
    {
        let mut w = tar::Writer::new(BufWriter::with_capacity(1 << 20, z.stdin.take().expect("piped")));
        for m in members {
            let err = |e: std::io::Error| format!("{}: {}: {e}", out.display(), m.path);
            match &m.from {
                Src::File(p) => {
                    let f = std::fs::File::open(p).map_err(|e| format!("{}: {e}", p.display()))?;
                    let meta = f.metadata().map_err(|e| format!("{}: {e}", p.display()))?;
                    use std::os::unix::fs::PermissionsExt;
                    let mode = if meta.permissions().mode() & 0o111 != 0 { 0o755 } else { 0o644 };
                    w.file(&m.path, mode, meta.len(), f).map_err(err)?;
                    bytes += meta.len();
                }
                Src::Bytes(b) => {
                    w.file(&m.path, 0o644, b.len() as u64, &b[..]).map_err(err)?;
                    bytes += b.len() as u64;
                }
            }
            files += 1;
        }
        w.finish().map_err(|e| format!("{}: {e}", out.display()))?;
    }
    let st = z.wait().map_err(|e| format!("zstd: {e}"))?;
    if !st.success() {
        return Err(format!("zstd failed ({st}) for {}", out.display()));
    }
    std::fs::rename(&tmp, out).map_err(|e| format!("{}: {e}", out.display()))?;
    Ok((bytes, files))
}

fn notice(root: &Path, what: &str) -> Result<Vec<Member>, String> {
    let mut v = vec![Member::bytes(
        "NOTICE.txt",
        format!("{what}\nMade by `cargo xtask release-assets` of {}. BitsOS's own code is MIT OR Apache-2.0 (LICENSE-MIT,\nLICENSE-APACHE, NOTICE.md beside this file); every other part carries its licence texts in its folder\n(LICENSES/, or /usr/share/licenses inside the initramfs).\n", rel::REPOSITORY).into_bytes(),
    )];
    for f in ["LICENSE-MIT", "LICENSE-APACHE", "NOTICE.md"] {
        v.push(Member::file(f, &root.join(f)));
    }
    Ok(v)
}

/// The firmware the drivers' lists name, decompressed with its licences (`firmware::copy`); a release needs all.
fn firmware(root: &Path, work: &Path) -> Result<Vec<Member>, String> {
    let _ = std::fs::remove_dir_all(work);
    crate::firmware::copy(root, work)?;
    let dir = work.join("BITSOS2/firmware");
    for list in [crate::firmware::LIST, "crates/drivers/rtw89/firmware.list"] {
        let text = std::fs::read_to_string(root.join(list)).map_err(|e| format!("{list}: {e}"))?;
        for name in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            if !dir.join(name).is_file() {
                return Err(format!("release-assets: firmware {name} is missing (linux-firmware-amdgpu/-realtek and zstd on this host?)"));
            }
        }
    }
    let mut files = Vec::new();
    walk(&dir, "BITSOS2/firmware", &mut files)?;
    Ok(files.into_iter().map(|(r, p)| Member::file(r, &p)).collect())
}

fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, PathBuf)>) -> Result<(), String> {
    let mut es: Vec<_> = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten().collect();
    es.sort_by_key(|e| e.file_name());
    for e in es {
        let r = format!("{rel}/{}", e.file_name().to_string_lossy());
        if e.path().is_dir() {
            walk(&e.path(), &r, out)?;
        } else {
            out.push((r, e.path()));
        }
    }
    Ok(())
}

fn parse_trees(v: Option<String>) -> Result<Vec<String>, String> {
    let all: Vec<String> = TREES.iter().chain(RUNTIMES).map(|(t, _)| t.to_string()).collect();
    match v.as_deref() {
        None | Some("all") => Ok(all),
        Some("none") => Ok(Vec::new()),
        Some(l) => {
            let w: Vec<String> = l.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            match w.iter().find(|t| !all.contains(t)) {
                Some(bad) => Err(format!("release-assets: '{bad}' is no tree of a release ({})", all.join(", "))),
                None => Ok(w),
            }
        }
    }
}

/// Reads a packed archive back with the installer's own reader and asks the installer where each member goes: a
/// release whose archive the installer could not unpack, or that holds a member without a place on the stick, would
/// only fail at a tester's desk, after the stick was erased.
fn check_archive(path: &Path, tree: Option<&str>, (bytes, files): (u64, u64)) -> Result<(), String> {
    let name = path.file_name().expect("a file").to_string_lossy().into_owned();
    let mut s = bitsos_install::archive::open(path)?;
    let (mut got_bytes, mut got_files) = (0u64, 0u64);
    while let Some(e) = s.next_entry().map_err(|e| format!("{name}: {e}"))? {
        if e.kind == tar::EntryKind::Dir {
            continue;
        }
        let placed = match tree {
            _ if e.path == "VENDOR.txt" => true,
            None => stick::system_dest(&e.path, &name).is_some(),
            Some(t) => stick::tree_dest(&e.path, t, &name).is_some(),
        };
        if !placed {
            return Err(format!("release-assets: {name} holds {}, which the installer has no place for", e.path));
        }
        let n = std::io::copy(&mut s, &mut std::io::sink()).map_err(|e| format!("{name}: {e}"))?;
        if n != e.size {
            return Err(format!("release-assets: {name}: {} reads back with {n} bytes, not {}", e.path, e.size));
        }
        got_bytes += n;
        got_files += 1;
    }
    if (got_bytes, got_files) != (bytes, files) {
        return Err(format!("release-assets: {name} reads back as {got_files} files and {got_bytes} bytes, packed were {files} and {bytes}"));
    }
    Ok(())
}

/// The files `release-assets` makes in its output folder; nothing else there is touched.
fn ours(name: &str) -> bool {
    name.starts_with("bitsos-") || name == bitsos_install::manifest::FILE || name == "SHA256SUMS"
}

fn asset_entry(path: &Path, kind: Kind, tree: Option<&str>, needs: &[&str], (unpacked, files): (u64, u64), (vendor_bytes, vendor_files): (u64, u64)) -> Result<Asset, String> {
    check_archive(path, tree, (unpacked, files))?;
    let (sha256, size) = sha::of_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if size > MAX_ASSET {
        return Err(format!("{}: {size} bytes, more than a GitHub release asset may have", path.display()));
    }
    let name = path.file_name().expect("a file").to_string_lossy().into_owned();
    println!("asset:    {name} ({} MB, {files} files, {} MB unpacked)", size >> 20, unpacked >> 20);
    Ok(Asset { name, size, sha256, kind, tree: tree.map(String::from), needs: needs.iter().map(|s| s.to_string()).collect(), unpacked, files, vendor_bytes, vendor_files })
}

/// Builds `bitsos-install` for `target` with the release's version and manifest hash.
fn installer(root: &Path, out: &Path, version: &str, hash: &str, target: &str, name: &str) -> Result<PathBuf, String> {
    let mut c = cargo(root);
    c.args(["build", "-p", "bitsos-install", "--release", "--target", target]).env("BITSOS_RELEASE_VERSION", version).env("BITSOS_RELEASE_MANIFEST_SHA256", hash);
    // A download for strangers: no debug info (the workspace keeps it for the kernel's backtraces).
    c.env("CARGO_PROFILE_RELEASE_DEBUG", "0").env("CARGO_PROFILE_RELEASE_STRIP", "symbols");
    run_status(&mut c, &format!("building bitsos-install for {target}"))?;
    let exe = if target.contains("windows") { "bitsos-install.exe" } else { "bitsos-install" };
    let to = out.join(name);
    std::fs::copy(root.join("target").join(target).join("release").join(exe), &to).map_err(|e| format!("{}: {e}", to.display()))?;
    Ok(to)
}

fn windows_ready() -> bool {
    let t = Command::new("rustup").args(["target", "list", "--installed"]).output().map(|o| String::from_utf8_lossy(&o.stdout).contains("x86_64-pc-windows-gnu")).unwrap_or(false);
    t && Command::new("x86_64-w64-mingw32-gcc").arg("--version").output().is_ok_and(|o| o.status.success())
}

pub fn cmd_release_assets(root: &Path, mut a: Args) -> Result<(), String> {
    if a.flag("--help") {
        println!("{HELP}");
        return Ok(());
    }
    let version = a.value("--version")?.ok_or(format!("release-assets needs --version V\n{HELP}"))?;
    if !bitsos_install::manifest::name_ok(&version) {
        return Err(format!("release-assets: bad version '{version}'"));
    }
    let out = a.value("--out")?.map(PathBuf::from).unwrap_or_else(|| root.join("target/release-assets"));
    let trees = parse_trees(a.value("--trees")?)?;
    let level = a.num::<u32>("--zstd-level")?.unwrap_or(12).clamp(1, 19);
    let no_installer = a.flag("--no-installer");
    a.positionals()?;
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    for e in std::fs::read_dir(&out).map_err(|e| e.to_string())?.flatten() {
        if e.path().is_file() && ours(&e.file_name().to_string_lossy()) {
            let _ = std::fs::remove_file(e.path());
        }
    }

    let art = image::build_with(root, BuildOpts { release: true, ktest: false }, true)?;
    // An image without them still boots; a release without a shell or Steam's container helper is not one.
    for need in ["busybox", "bwrap"] {
        if !art.programs.iter().any(|(n, _)| n == need) {
            return Err(format!("release-assets: the build has no /bin/{need}: build the musl sysroot first (user/sysroot/build.sh); the port's tarball must be in the download cache"));
        }
    }
    let initrd = image::initrd_bytes_for(root, &art.programs, &art.kernel)?;
    let mut members = vec![
        Member::file("EFI/BOOT/BOOTX64.EFI", &art.bitsboot),
        Member::file("SYS/KERNEL", &art.kernel),
        Member::bytes("SYS/INITRD", initrd),
    ];
    members.extend(firmware(root, &xdir(root).join("release-work"))?);
    members.extend(art.stick.iter().filter(|(r, _)| !crate::image::package::is_tree_file(r)).map(|(r, p)| Member::file(r.clone(), p)));
    members.extend(notice(root, &format!("BitsOS {version}: the system (bitsboot, kernel, initramfs, firmware, \\BITSOS2\\bin\\)."))?);
    let sys = out.join(rel::asset_name("system", &version));
    let mut assets = vec![asset_entry(&sys, Kind::System, None, &[], pack(&sys, &members, level)?, (0, 0))?];

    for (tree, needs) in TREES.iter().chain(RUNTIMES).filter(|(t, _)| trees.iter().any(|w| w == t)) {
        let prefix = format!("BITSOS2/{tree}/");
        let files: Vec<(String, PathBuf)> = art.stick.iter().filter(|(r, _)| r.starts_with(&prefix)).cloned().collect();
        if files.is_empty() {
            return Err(format!("release-assets: the build has no {tree} tree (its stage is missing; see user/packaging/manifest.txt)"));
        }
        let list = image::tree_list_texts(files.iter().map(|(r, p)| (r.as_str(), p.clone())))?.remove(&format!("BITSOS2/{tree}")).unwrap_or_default();
        let runtime = RUNTIMES.iter().any(|(t, _)| t == tree);
        let (keep, vendor_txt) = if runtime { vendor::split(root, tree, &files)? } else { (files.clone(), None) };
        let vendor_size = match &vendor_txt {
            Some(v) => bitsos_install::vendor::List::parse(v).map(|l| (l.bytes(), l.files.len() as u64))?,
            None => (0, 0),
        };
        let mut m: Vec<Member> = keep.iter().map(|(r, p)| Member::file(r.clone(), p)).collect();
        m.push(Member::bytes(format!("{prefix}PKGLIST.TXT"), list.into_bytes()));
        if let Some(v) = vendor_txt {
            m.push(Member::bytes("VENDOR.txt", v.into_bytes()));
        }
        let what = if runtime { format!("{tree}-runtime") } else { tree.to_string() };
        m.extend(notice(root, &format!("BitsOS {version}: the tree \\BITSOS2\\{tree}\\{}.", if runtime { " without its maker's files (VENDOR.txt names them)" } else { "" }))?);
        let p = out.join(rel::asset_name(&what, &version));
        assets.push(asset_entry(&p, if runtime { Kind::Runtime } else { Kind::Tree }, Some(tree), needs, pack(&p, &m, level)?, vendor_size)?);
    }

    let manifest = Manifest { version: version.clone(), assets }.to_json();
    Manifest::parse(&manifest)?;
    std::fs::write(out.join(bitsos_install::manifest::FILE), &manifest).map_err(|e| e.to_string())?;
    let hash = sha::hex(manifest.as_bytes());
    println!("manifest: {} sha256 {hash}", out.join(bitsos_install::manifest::FILE).display());
    if !no_installer {
        let l = installer(root, &out, &version, &hash, "x86_64-unknown-linux-musl", &format!("bitsos-install-{version}-linux-x86_64"))?;
        println!("installer: {}", l.display());
        if windows_ready() {
            let w = installer(root, &out, &version, &hash, "x86_64-pc-windows-gnu", &format!("bitsos-install-{version}-windows-x86_64.exe"))?;
            println!("installer: {} (built, never run on Windows by this tool)", w.display());
        } else {
            println!("note:     no Windows installer: rustup target x86_64-pc-windows-gnu or x86_64-w64-mingw32-gcc is missing");
        }
    }
    let mut sums = String::new();
    let mut names: Vec<String> = std::fs::read_dir(&out).map_err(|e| e.to_string())?.flatten().filter(|e| e.path().is_file()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| ours(n) && !n.ends_with(".part")).collect();
    names.sort();
    for n in names.iter().filter(|n| *n != "SHA256SUMS") {
        sums.push_str(&format!("{}  {n}\n", sha::of_file(&out.join(n)).map_err(|e| e.to_string())?.0));
    }
    std::fs::write(out.join("SHA256SUMS"), sums).map_err(|e| e.to_string())?;
    println!("release-assets: {} ready; upload every file there to the release v{version} of {}", out.display(), rel::REPOSITORY);
    println!("  an installer built by hand for it: BITSOS_RELEASE_VERSION={version} BITSOS_RELEASE_MANIFEST_SHA256={hash} cargo build -p bitsos-install --release");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_archives_read_back_with_the_installers_reader() {
        let d = std::env::temp_dir().join(format!("xt-release-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("k"), b"kernel").unwrap();
        let out = d.join("a.tar.zst");
        let (bytes, files) = pack(&out, &[Member::file("SYS/KERNEL", &d.join("k")), Member::bytes("BITSOS2/bin/x", vec![7; 70_000])], 3).unwrap();
        assert_eq!((bytes, files), (70_006, 2));
        let mut s = bitsos_install::archive::open(&out).unwrap();
        let mut got = Vec::new();
        while let Some(e) = s.next_entry().unwrap() {
            let mut b = Vec::new();
            std::io::Read::read_to_end(&mut s, &mut b).unwrap();
            got.push((e.path, b.len()));
        }
        assert_eq!(got, vec![("SYS/KERNEL".to_string(), 6), ("BITSOS2/bin/x".to_string(), 70_000)]);
        check_archive(&out, None, (bytes, files)).unwrap();
        assert!(check_archive(&out, None, (bytes, files + 1)).unwrap_err().contains("reads back"));
        // A tree asset whose member lies outside its tree: refused at release time, not on a stick.
        let bad = d.join("bitsos-mesa-1.tar.zst");
        let packed = pack(&bad, &[Member::bytes("BITSOS2/mesa/lib/a.so", vec![1; 10]), Member::bytes("BITSOS2/bin/evil", vec![2; 5])], 12).unwrap();
        assert!(check_archive(&bad, Some("mesa"), packed).unwrap_err().contains("no place"));
        let _ = std::fs::remove_dir_all(&d);
        assert!(ours("bitsos-system-1.tar.zst") && ours("manifest.json") && ours("SHA256SUMS") && ours("bitsos-install-1-linux-x86_64"));
        assert!(!ours("holiday.jpg") && !ours("notes.txt"), "a foreign file in --out stays");
        assert_eq!(parse_trees(Some("none".into())).unwrap(), Vec::<String>::new());
        assert!(parse_trees(Some("cobalt".into())).is_err(), "Cobalt is not passed on");
        assert!(!parse_trees(None).unwrap().iter().any(|t| t == "deqp" || t == "cobalt"));
    }
}
