//! build, initrd and image: kernel + loader + user programs -> cpio -> ESP tree -> FAT32 image.
use crate::cpio;
use crate::util::*;
#[path = "package.rs"]
pub mod package;
use std::path::{Path, PathBuf};
use std::process::Command;

const DEFAULT_CONFIG: &str = include_str!("../default-config.txt");
/// Fixed identity of the test stick (32 hex digits, no newline).
pub const TEST_STICK_ID: &str = "00112233445566778899aabbccddeeff";

#[derive(Clone, Copy, Default)]
pub struct BuildOpts {
    pub release: bool,
    pub ktest: bool,
}

impl BuildOpts {
    pub fn parse(a: &mut Args) -> BuildOpts {
        BuildOpts { release: a.flag("--release"), ktest: a.flag("--ktest") }
    }
    pub fn profile(&self) -> &'static str {
        if self.release { "release" } else { "debug" }
    }
}

pub struct Artefacts {
    pub kernel: PathBuf,
    pub bitsboot: PathBuf,
    /// (name under /bin, file); a library of the set (a name with `.so`) goes to its Linux path instead
    /// (`package::lib_dest`: /usr/lib, the loader too; /lib64 links there).
    pub programs: Vec<(String, PathBuf)>,
    /// UL-5: files for the stick (path below its root, file), from `user/packaging/manifest.txt`: programs in
    /// `BITSOS2/bin`, YT-2's trees in `BITSOS2/<dir>` (V2: `/boot/...`).
    pub stick: Vec<(String, PathBuf)>,
}

pub fn build(root: &Path, o: BuildOpts) -> Result<Artefacts, String> {
    let kernel = build_kernel(root, o)?;
    println!("kernel:   {}", kernel.display());

    let mut b = cargo(root);
    b.args(["build", "-p", "bitsboot", "--target", "x86_64-unknown-uefi"]);
    if o.release {
        b.arg("--release");
    }
    run_status(&mut b, "building bitsboot")?;
    let bitsboot = root.join("target/x86_64-unknown-uefi").join(o.profile()).join("bitsboot.efi");
    println!("bitsboot: {}", bitsboot.display());

    let (programs, stick) = build_packaged(root)?;
    for (n, p) in &programs {
        let at = package::lib_dest(n).unwrap_or_else(|| format!("bin/{n}"));
        println!("user:     /{at} <- {}", p.display());
    }
    if programs.is_empty() {
        println!("user:     (no user programs)");
    }
    for l in package::stick_summary(&stick) {
        println!("stick:    {l}");
    }
    Ok(Artefacts { kernel, bitsboot, programs, stick })
}

/// The user programs plus the package (UL-5): the manifest's programs and the glibc library set, sorted.
fn build_packaged(root: &Path) -> Result<(Vec<(String, PathBuf)>, Vec<(String, PathBuf)>), String> {
    let mut programs = build_user(root)?;
    let pkg = package::build(root, &programs)?;
    programs.extend(pkg.initrd);
    programs.sort();
    Ok((programs, pkg.stick))
}

/// Builds the kernel and copies it to a path of its own per profile and feature set
/// (`target/xtask/kernel-<profile>-<ktest|plain>`). Cargo writes both feature sets to the same file, so parallel
/// scenario chains (`fullrun`) building `--ktest` and plain images raced and booted the wrong kernel (US-2 change
/// request): the build and the copy run under one file lock, and the copy lands by rename (readers see a whole
/// file).
fn build_kernel(root: &Path, o: BuildOpts) -> Result<PathBuf, String> {
    let x = xdir(root);
    std::fs::create_dir_all(&x).map_err(|e| format!("{}: {e}", x.display()))?;
    let lock_path = x.join("kernel-build.lock");
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lock_path).map_err(|e| format!("{}: {e}", lock_path.display()))?;
    lock.lock().map_err(|e| format!("lock {}: {e}", lock_path.display()))?;
    let mut k = cargo(root);
    k.args(["build", "-p", "kernel", "--target", "x86_64-unknown-none"]);
    if o.release {
        k.arg("--release");
    }
    if o.ktest {
        k.args(["--features", "ktest"]);
    }
    run_status(&mut k, "building kernel")?;
    let built = root.join("target/x86_64-unknown-none").join(o.profile()).join("kernel");
    let out = x.join(format!("kernel-{}-{}", o.profile(), if o.ktest { "ktest" } else { "plain" }));
    let tmp = x.join(format!("kernel-{}-{}.tmp{}", o.profile(), if o.ktest { "ktest" } else { "plain" }, std::process::id()));
    std::fs::copy(&built, &tmp).map_err(|e| format!("copy {} -> {}: {e}", built.display(), tmp.display()))?;
    std::fs::rename(&tmp, &out).map_err(|e| format!("rename {} -> {}: {e}", tmp.display(), out.display()))?;
    drop(lock);
    Ok(out)
}

fn is_elf(p: &Path) -> bool {
    std::fs::read(p).map(|b| b.starts_with(b"\x7fELF")).unwrap_or(false)
}

/// Test-only programs a V2_LEAN image leaves out (the ABI-diff set is skipped as a whole).
const LEAN_DROP: &[&str] = &["hello", "hello-rs", "abi-a", "abi-b", "abi-c", "alsa-test", "evtest", "fbtest", "mcp-fakesvc", "memstress", "stress"];

fn build_user(root: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let user = root.join("user");
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    if user.join("Cargo.toml").is_file() {
        let tdir = root.join("target/user");
        // The user workspace has its own toolchain file (stable); run from its directory so it applies.
        let mut c = Command::new("cargo");
        c.current_dir(&user)
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("RUSTUP_TOOLCHAIN")
            .args(["build", "--manifest-path", "Cargo.toml", "--target", "x86_64-unknown-linux-musl", "--release", "--target-dir"])
            .arg(&tdir);
        run_status(&mut c, "building user programs")?;
        let rel = tdir.join("x86_64-unknown-linux-musl/release");
        if let Ok(rd) = std::fs::read_dir(&rel) {
            for e in rd.flatten() {
                let p = e.path();
                let name = e.file_name().to_string_lossy().to_string();
                if p.is_file() && p.extension().is_none() && !name.starts_with('.') && is_elf(&p) {
                    out.push((name, p));
                }
            }
        }
    } else {
        eprintln!("xtask: no user/Cargo.toml yet, skipping Rust user programs");
    }
    out.extend(build_c(root));
    // AB-3 change request 3 (INT-A1): AB's ABI-diff programs (user/abi-tests/{c,rust}) in every image, so the
    // Mini-PC can `run abi-time` and `abi-diff --remote` diffs real hardware. They need UL-1's musl sysroot; only
    // when it is already built (`cargo xtask abi-diff` builds it once) or V2_SYSROOT names one, so a plain build
    // never starts the long sysroot build. A failure leaves them out with a warning.
    let sysroot_ready = std::env::var_os("V2_SYSROOT").is_some() || root.join("user/sysroot/out/sysroot/usr/lib/libc.a").is_file();
    // V2_LEAN=1 (desk 24.10., Georg's "richtige Endfassung"): the everyday stick image without test programs.
    let lean = std::env::var_os("V2_LEAN").is_some();
    if lean {
        out.retain(|(n, _)| !LEAN_DROP.contains(&n.as_str()));
    }
    if sysroot_ready && !lean {
        match crate::abi_diff::build_programs(root) {
            Ok(p) => {
                for (n, f) in p {
                    if !out.iter().any(|(m, _)| *m == n) {
                        out.push((n, f));
                    }
                }
            }
            Err(e) => eprintln!("xtask: warning: ABI-diff programs not in the image: {e}"),
        }
    } else {
        eprintln!("xtask: note: no musl sysroot yet (user/sysroot/out): the ABI-diff programs are not in the image (cargo xtask abi-diff builds it once)");
    }
    if sysroot_ready {
        if let Some(bb) = build_busybox(root) {
            out.retain(|(n, _)| n != "busybox");
            out.push(("busybox".to_string(), bb));
        }
    }
    out.sort();
    Ok(out)
}

/// UL-3a: BusyBox from user/ports/busybox (build.sh is idempotent and never downloads here: V2_OFFLINE=1; its
/// README says how the one tarball gets into the download cache). Missing tarball or a failed build: not in the
/// image, with a note; the ul.bb.* ktests then skip.
fn build_busybox(root: &Path) -> Option<PathBuf> {
    let dir = root.join("user/ports/busybox");
    if !dir.join("build.sh").is_file() {
        return None;
    }
    let o = Command::new("sh").arg(dir.join("build.sh")).env("V2_OFFLINE", "1").output();
    match o {
        Ok(o) if o.status.success() => Some(dir.join("out/busybox")),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            eprintln!("xtask: note: busybox not in the image: {}", err.lines().last().unwrap_or("build.sh failed"));
            None
        }
        Err(e) => {
            eprintln!("xtask: note: busybox not in the image: {e}");
            None
        }
    }
}

/// Every user/c/*.c (the smoke programs) and host/tests/fs/progs/*.c (FS's syscall case runner, ktest
/// fs.calls.linux_cases; FS-1 change request 3) as a static musl program using the self-contained musl pieces of
/// the Rust sysroot. A program that does not link is skipped with a warning; the full sysroot is item UL-1.
fn build_c(root: &Path) -> Vec<(String, PathBuf)> {
    let mut srcs: Vec<PathBuf> = Vec::new();
    for cdir in [root.join("user/c"), root.join("host/tests/fs/progs")] {
        if let Ok(rd) = std::fs::read_dir(&cdir) {
            srcs.extend(rd.flatten().map(|e| e.path()).filter(|p| p.extension().map(|x| x == "c").unwrap_or(false)));
        }
    }
    srcs.sort();
    if srcs.is_empty() {
        return Vec::new();
    }
    let sysroot = Command::new("rustc").current_dir(root).args(["--print", "sysroot"]).output().ok();
    let Some(sysroot) = sysroot.filter(|o| o.status.success()) else {
        eprintln!("xtask: warning: rustc --print sysroot failed, skipping C programs");
        return Vec::new();
    };
    let sc = PathBuf::from(String::from_utf8_lossy(&sysroot.stdout).trim())
        .join("lib/rustlib/x86_64-unknown-linux-musl/lib/self-contained");
    let odir = crate::util::xdir(root).join("cbin");
    let _ = std::fs::create_dir_all(&odir);
    let mut out = Vec::new();
    for s in srcs {
        let name = s.file_stem().unwrap().to_string_lossy().to_string();
        let exe = odir.join(&name);
        let st = Command::new("clang")
            .args(["--target=x86_64-linux-gnu", "-static", "-nostdlib", "-O2", "-fno-stack-protector", "-fno-pie"])
            .arg(sc.join("crt1.o"))
            .arg(sc.join("crti.o"))
            .arg(&s)
            .arg(sc.join("libc.a"))
            .arg(sc.join("crtn.o"))
            .arg("-o")
            .arg(&exe)
            .status();
        match st {
            Ok(x) if x.success() => out.push((name, exe)),
            _ => eprintln!("xtask: warning: C program {} did not build/link with the self-contained musl; skipped (full sysroot is UL-1)", s.display()),
        }
    }
    out
}

fn initrd_entries(root: &Path, programs: &[(String, PathBuf)]) -> Result<Vec<cpio::Entry>, String> {
    let mut v = Vec::new();
    for (n, p) in programs {
        let data = std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))?;
        let dest = package::lib_dest(n).unwrap_or_else(|| format!("bin/{n}"));
        v.push(cpio::file(&dest, 0o755, data));
    }
    // BusyBox's applets as links next to it (out/applets beside the binary); a real program of the same name wins.
    if let Some((_, bb)) = programs.iter().find(|(n, _)| n == "busybox") {
        let list = std::fs::read_to_string(bb.with_file_name("applets")).unwrap_or_default();
        for a in list.lines().map(str::trim).filter(|a| !a.is_empty() && !a.contains('/')) {
            if !programs.iter().any(|(n, _)| n == a) {
                v.push(cpio::symlink(&format!("bin/{a}"), "busybox"));
            }
        }
    }
    // UL-5: merged-/usr links, the UL-4a CA bundle (/etc/ssl/certs/ca-certificates.crt, where musl programs and
    // fetch look first), licence texts, /usr/share/bitsos/package.txt.
    v.extend(package::initrd_extra(root, programs)?);
    let extra = root.join("host/xtask/initrd-extra");
    if extra.is_dir() {
        add_dir(&extra, &extra, &mut v)?;
    }
    Ok(v)
}

fn add_dir(base: &Path, d: &Path, v: &mut Vec<cpio::Entry>) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut es: Vec<_> = std::fs::read_dir(d).map_err(|e| e.to_string())?.flatten().collect();
    es.sort_by_key(|e| e.file_name());
    for e in es {
        let p = e.path();
        let rel = p.strip_prefix(base).unwrap().to_string_lossy().to_string();
        if p.is_dir() {
            add_dir(base, &p, v)?;
        } else if p.is_file() {
            let perm = std::fs::metadata(&p).map_err(|e| e.to_string())?.permissions().mode() & 0o777;
            v.push(cpio::file(&rel, perm, std::fs::read(&p).map_err(|e| e.to_string())?));
        }
    }
    Ok(())
}

/// RB-2: the initramfs with `boot/kernel.syms`, the code symbols of exactly this kernel file (`nm -n -C
/// --defined-only`, lines of type t/T/w/W), so backtraces name functions after every boot and every Reload.
/// Without `nm` the initramfs is made without it (backtraces show addresses) and xtask says so.
pub fn initrd_bytes_for(root: &Path, programs: &[(String, PathBuf)], kernel: &Path) -> Result<Vec<u8>, String> {
    let mut v = initrd_entries(root, programs)?;
    match Command::new("nm").args(["-n", "-C", "--defined-only"]).arg(kernel).output() {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            let mut syms = String::new();
            for l in text.lines() {
                let b = l.as_bytes();
                if b.len() > 19 && b[16] == b' ' && b[18] == b' ' && matches!(b[17], b't' | b'T' | b'w' | b'W') {
                    syms.push_str(l);
                    syms.push('\n');
                }
            }
            v.push(cpio::file("boot/kernel.syms", 0o644, syms.into_bytes()));
        }
        _ => eprintln!("xtask: warning: nm failed on {}: no boot/kernel.syms (backtraces show addresses only)", kernel.display()),
    }
    Ok(cpio::write(&v))
}

pub fn cmd_initrd(root: &Path, mut a: Args) -> Result<(), String> {
    let out = a.value("--out")?.map(PathBuf::from).unwrap_or_else(|| xdir(root).join("initrd.cpio"));
    let lock = a.flag("--lock");
    let o = BuildOpts::parse(&mut a);
    a.positionals()?;
    let (programs, stick) = build_packaged(root)?;
    let _ = o;
    std::fs::create_dir_all(out.parent().unwrap_or(Path::new("."))).map_err(|e| e.to_string())?;
    let entries = initrd_entries(root, &programs)?;
    let list = entries.iter().find(|e| e.name == package::LIST).map(|e| String::from_utf8_lossy(&e.data).into_owned()).unwrap_or_default();
    let bytes = cpio::write(&entries);
    std::fs::write(&out, &bytes).map_err(|e| e.to_string())?;
    println!("initrd: {} ({} bytes, {} programs, {} for the stick)", out.display(), bytes.len(), programs.len(), stick.len());
    if lock {
        let n = package::write_lock(root, &list)?;
        println!("initrd: {} written ({n} libraries)", package::LOCK);
    }
    Ok(())
}

pub struct ImageOpts {
    pub name: String,
    pub generation: u32,
    pub config_extra: Vec<String>,
    pub build: BuildOpts,
}

impl ImageOpts {
    pub fn parse(a: &mut Args) -> Result<ImageOpts, String> {
        let name = a.value("--name")?.unwrap_or_else(|| "test".into());
        let generation = a.num::<u32>("--gen")?.unwrap_or(1);
        let extra = a.value("--config-extra")?.unwrap_or_default();
        let config_extra = extra.split(';').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
        Ok(ImageOpts { name, generation, config_extra, build: BuildOpts::parse(a) })
    }
}

pub fn safe_name(n: &str) -> Result<(), String> {
    if n.is_empty() || !n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(format!("name '{n}': only letters, digits, - and _ are allowed"));
    }
    Ok(())
}

pub fn config_text(extra: &[String]) -> String {
    let mut s = DEFAULT_CONFIG.to_string();
    if !s.ends_with('\n') {
        s.push('\n');
    }
    for l in extra {
        s.push_str(l);
        s.push('\n');
    }
    s
}

/// Builds everything and returns the path of `target/xtask/<name>.img`.
pub fn make_image(root: &Path, o: &ImageOpts) -> Result<PathBuf, String> {
    safe_name(&o.name)?;
    let art = build(root, o.build)?;
    make_image_from(root, o, &art)
}

pub fn make_image_from(root: &Path, o: &ImageOpts, art: &Artefacts) -> Result<PathBuf, String> {
    safe_name(&o.name)?;
    let x = xdir(root);
    let esp = x.join(format!("esp-{}", o.name));
    let _ = std::fs::remove_dir_all(&esp);
    let sys = esp.join(format!("BITSOS2/SYS/{}", o.generation));
    let boot = esp.join("EFI/BOOT");
    for d in [&sys, &boot] {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let cp = |from: &Path, to: PathBuf| std::fs::copy(from, &to).map(|_| ()).map_err(|e| format!("copy {} -> {}: {e}", from.display(), to.display()));
    cp(&art.bitsboot, boot.join("BOOTX64.EFI"))?;
    cp(&art.kernel, sys.join("KERNEL"))?;
    std::fs::write(sys.join("INITRD"), initrd_bytes_for(root, &art.programs, &art.kernel)?).map_err(|e| e.to_string())?;
    std::fs::write(esp.join("BITSOS2/CONFIG.TXT"), config_text(&o.config_extra)).map_err(|e| e.to_string())?;
    std::fs::write(esp.join("BITSOS2/STICK.ID"), TEST_STICK_ID).map_err(|e| e.to_string())?;
    // UL-5: the manifest's stick programs in \BITSOS2\bin (V2: /boot/BITSOS2/bin); YT-2's trees as `tree_wanted`
    // says, so the other scenario images stay small.
    let env = std::env::var("V2_STICK_TREES").ok();
    for (rel, p) in art.stick.iter().filter(|(rel, _)| !package::is_tree_file(rel) || tree_wanted(rel, o, env.as_deref())) {
        let to = esp.join(rel);
        if let Some(d) = to.parent() {
            std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
        }
        cp(p, to)?;
    }

    let img = x.join(format!("{}.img", o.name));
    let _ = std::fs::remove_file(&img);
    // At least 65536 KiB: the smallest size where mkfs.fat -F 32 gives a valid FAT32 (>= 65525 clusters at
    // 512 B x 1); larger when the files need it (a debug kernel with full debug info passed 64 MiB in wave 3).
    let kib = image_kib(dir_bytes(&esp));
    run_status(Command::new("mkfs.fat").args(["-F", "32", "-n", "BITSOS", "-C"]).arg(&img).arg(kib.to_string()), "mkfs.fat (image file only)")?;
    let mut mc = Command::new("mcopy");
    mc.args(["-s", "-i"]).arg(&img);
    for e in ["EFI", "BITSOS2"] {
        mc.arg(esp.join(e));
    }
    mc.arg("::/");
    run_status(&mut mc, "mcopy into image")?;
    println!("image:    {} (ESP tree {})", img.display(), esp.display());
    Ok(img)
}

/// Whether the tree file `rel` (`BITSOS2/<tree>/...`) goes into image `o`: every tree with V2_STICK_TREES=1, the
/// named ones with V2_STICK_TREES=<tree>,<tree>; else only in a ktest image whose filter (comma-separated prefixes,
/// empty = all) can run the tree's own tests, named `ul.pkg.<tree>*` (BR-2: the gate's all-tests image and
/// `ktest ul.pkg` carry Brave's 500 MB and Cobalt's 190 MB; a scenario image for rb.mirror or cp.irq does not).
fn tree_wanted(rel: &str, o: &ImageOpts, env: Option<&str>) -> bool {
    let tree = rel.split('/').nth(1).unwrap_or("");
    match env.map(str::trim) {
        Some("1") => return true,
        Some(l) if l.split(',').any(|n| n.trim().eq_ignore_ascii_case(tree)) => return true,
        _ => {}
    }
    if !o.build.ktest {
        return false;
    }
    let filter = o.config_extra.iter().rev().find_map(|e| e.strip_prefix("ktest=")).unwrap_or("").trim();
    let own = format!("ul.pkg.{}", tree.to_ascii_lowercase());
    filter.is_empty() || filter.split(',').map(str::trim).filter(|p| !p.is_empty()).any(|p| own.starts_with(p) || p.starts_with(&own))
}

/// Bytes of every file below `dir`.
fn dir_bytes(dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_bytes(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

/// Image size in KiB for `content` bytes of files: room for FAT overhead and a second kernel copy (a test
/// generation), never below FAT32's minimum, rounded up to whole MiB.
fn image_kib(content: u64) -> u64 {
    let want = content + content / 4 + (16 << 20);
    (want.div_ceil(1 << 20) * 1024).max(65536)
}

pub fn cmd_image(root: &Path, mut a: Args) -> Result<(), String> {
    let o = ImageOpts::parse(&mut a)?;
    a.positionals()?;
    make_image(root, &o).map(|_| ())
}

pub fn cmd_build(root: &Path, mut a: Args) -> Result<(), String> {
    let o = BuildOpts::parse(&mut a);
    a.positionals()?;
    build(root, o).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_has_defaults_then_extras() {
        let c = config_text(&["ktest=mm".into(), "x=1".into()]);
        assert!(c.contains("loglevel=info") && c.contains("panic=halt"));
        assert!(c.trim_end().ends_with("ktest=mm\nx=1"));
    }

    #[test]
    fn image_grows_with_its_files() {
        assert_eq!(image_kib(0), 65536);
        assert_eq!(image_kib(10 << 20), 65536);
        let k = image_kib(80 << 20);
        assert!(k >= 116 * 1024 && k % 1024 == 0, "{k}");
    }

    #[test]
    fn names_are_checked() {
        assert!(safe_name("ktest-1_a").is_ok());
        assert!(safe_name("../x").is_err());
        assert!(safe_name("").is_err());
    }

    #[test]
    fn stick_id_is_32_hex() {
        assert_eq!(TEST_STICK_ID.len(), 32);
        assert!(TEST_STICK_ID.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn image_has_the_stick_layout() {
        if !have_tool("mkfs.fat") || !have_tool("mcopy") || !have_tool("mdir") {
            eprintln!("mtools/mkfs.fat missing, skipped");
            return;
        }
        let root = std::env::temp_dir().join(format!("xt-image-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("k"), b"KERNEL-BYTES").unwrap();
        std::fs::write(root.join("b.efi"), b"MZ").unwrap();
        std::fs::write(root.join("prog"), b"\x7fELFxx").unwrap();
        let art = Artefacts { kernel: root.join("k"), bitsboot: root.join("b.efi"), programs: vec![("prog".into(), root.join("prog"))], stick: vec![("BITSOS2/bin/big".into(), root.join("prog")), ("BITSOS2/tree/app/x".into(), root.join("prog"))] };
        let o = ImageOpts { name: "t1".into(), generation: 3, config_extra: vec!["ktest=mm".into()], build: BuildOpts::default() };
        let img = make_image_from(&root, &o, &art).unwrap();
        let ls = |dir: &str| {
            let out = Command::new("mdir").args(["-b", "-i"]).arg(&img).arg(dir).output().unwrap();
            String::from_utf8_lossy(&out.stdout).to_string()
        };
        let sys = ls("::/BITSOS2/SYS/3");
        assert!(sys.contains("KERNEL") && sys.contains("INITRD"), "{sys}");
        assert!(ls("::/EFI/BOOT").contains("BOOTX64.EFI"));
        assert!(ls("::/BITSOS2/bin").contains("big"));
        let top = ls("::/BITSOS2");
        assert!(top.contains("CONFIG.TXT") && top.contains("STICK.ID"), "{top}");
        let cfg = Command::new("mtype").arg("-i").arg(&img).arg("::/BITSOS2/CONFIG.TXT").output().unwrap();
        assert!(String::from_utf8_lossy(&cfg.stdout).contains("ktest=mm"));
        // YT-2: a tree only in a ktest image (or with V2_STICK_TREES=1).
        assert!(!top.contains("tree"), "{top}");
        let o = ImageOpts { name: "t2".into(), generation: 1, config_extra: vec![], build: BuildOpts { release: false, ktest: true } };
        let img = make_image_from(&root, &o, &art).unwrap();
        let out = Command::new("mdir").args(["-b", "-i"]).arg(&img).arg("::/BITSOS2/tree/app").output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("x"), "{}", String::from_utf8_lossy(&out.stdout));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// BR-2: which images carry a tree.
    #[test]
    fn trees_only_where_their_tests_run() {
        let img = |ktest: bool, extra: &[&str]| ImageOpts { name: "t".into(), generation: 1, config_extra: extra.iter().map(|s| s.to_string()).collect(), build: BuildOpts { release: false, ktest } };
        let f = "BITSOS2/brave/lib/libgtk-3.so.0";
        for extra in [&[][..], &["ktest="], &["ktest=ul"], &["ktest=ul.pkg"], &["ktest=ul.pkg.brave"], &["ktest=ul.pkg.brave_sha"], &["loglevel=info", "ktest=cp.irq, ul.pkg.b"]] {
            assert!(tree_wanted(f, &img(true, extra), None), "{extra:?}");
        }
        for extra in [&["ktest=rb.mirror"][..], &["ktest=ul.pkg.cobalt"], &["ktest=ul.pkg.stick,cp"]] {
            assert!(!tree_wanted(f, &img(true, extra), None), "{extra:?}");
        }
        assert!(!tree_wanted(f, &img(false, &[]), None) && !tree_wanted(f, &img(false, &[]), Some("cobalt")));
        assert!(tree_wanted(f, &img(false, &[]), Some("1")) && tree_wanted(f, &img(false, &[]), Some("cobalt, Brave")));
        assert!(tree_wanted("BITSOS2/cobalt/loader_app", &img(true, &["ktest=ul.pkg.cobalt_profile"]), None));
    }

    /// UL-5: the same inputs give the same initramfs bytes, the package list is sorted by path and complete
    /// (every program and library with its SHA-256), and the libraries sit in usr/lib behind CachyOS's links.
    #[test]
    fn package_is_reproducible() {
        let root = std::env::temp_dir().join(format!("xt-repro-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut programs = Vec::new();
        for (n, body) in [("hello", &b"\x7fELF hello"[..]), ("ld-linux-x86-64.so.2", b"loader"), ("libc.so.6", b"libc")] {
            std::fs::write(root.join(n), body).unwrap();
            programs.push((n.to_string(), root.join(n)));
        }
        let one = crate::cpio::write(&initrd_entries(&root, &programs).unwrap());
        let two = crate::cpio::write(&initrd_entries(&root, &programs).unwrap());
        assert!(one == two, "two builds of the same inputs differ");
        let e = crate::cpio::read(&one).unwrap();
        let get = |n: &str| e.iter().find(|x| x.name == n).unwrap_or_else(|| panic!("no {n}"));
        assert_eq!(get("lib64").data, b"usr/lib");
        assert_eq!(get("lib").data, b"usr/lib");
        assert_eq!(get("usr/lib64").data, b"lib");
        assert_eq!(get("usr/lib/ld-linux-x86-64.so.2").data, b"loader");
        assert!(!e.iter().any(|x| x.name.starts_with("lib64/")));
        let list = String::from_utf8(get(package::LIST).data.clone()).unwrap();
        let paths: Vec<&str> = list.lines().filter(|l| !l.starts_with('#') && !l.starts_with("mode")).filter_map(|l| l.split_whitespace().nth(1)).collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
        for (p, body) in [("/bin/hello", &b"\x7fELF hello"[..]), ("/usr/lib/libc.so.6", b"libc"), ("/usr/lib/ld-linux-x86-64.so.2", b"loader")] {
            let want = format!(" {p} {} {}", body.len(), package::sha256(body));
            assert!(list.lines().any(|l| l.contains(&want)), "{p} not listed with its hash:\n{list}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}