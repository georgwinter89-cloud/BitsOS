//! Which test scripts need the musl sysroot before they run (L7-RESTE-AB 4). A script that builds or boots a V2
//! image gets busybox, /bin/sh and the ABI programs in it only when the sysroot exists at the image build
//! (`image::cmd_image` leaves them out otherwise); in a fresh worktree the gate used to build the sysroot only at
//! its ktest step, after the wy scripts that boot V2. A script
//! that only compiles against the sysroot (ul/20-busybox-linux) needs it too: without it, it says SKIP and passes.
use std::path::{Path, PathBuf};

/// The xtask commands that build or start a V2 image.
const IMAGE_COMMANDS: &[&str] = &["xtask image", "xtask qemu", "xtask ktest", "xtask stick", "xtask release-assets"];

/// What a script that builds against the sysroot itself names (its default place, the variable that moves it, its
/// compiler); without the sysroot such a script prints SKIP and passes without testing anything.
const SYSROOT_NAMES: &[&str] = &["user/sysroot/out/sysroot", "V2_SYSROOT", "sysroot/bin/v2-cc"];

/// Whether `script` (or a file it sources, three levels deep) runs one of the image commands or uses the sysroot.
pub fn needs_sysroot(root: &Path, script: &Path) -> bool {
    scan(root, script, 3)
}

fn scan(root: &Path, file: &Path, depth: u32) -> bool {
    let Ok(text) = std::fs::read_to_string(file) else { return false };
    if IMAGE_COMMANDS.iter().chain(SYSROOT_NAMES).any(|c| text.contains(c)) {
        return true;
    }
    depth > 0 && text.lines().filter_map(|l| sourced(root, file, l)).any(|f| scan(root, &f, depth - 1))
}

/// The file a `. "<path>"` or `source <path>` line reads: below `host/tests/` from the root when the path names
/// that folder, else next to the script (`$HERE`, `$(dirname "$0")`).
fn sourced(root: &Path, file: &Path, line: &str) -> Option<PathBuf> {
    let l = line.trim_start();
    let rest = l.strip_prefix(". ").or_else(|| l.strip_prefix("source "))?;
    // The whole rest: `"$(dirname "$0")/lib.bash"` has a blank inside its quotes.
    let word = rest.trim().trim_matches(|c| c == '"' || c == '\'');
    if let Some(i) = word.find("host/tests/") {
        return Some(root.join(&word[i..]));
    }
    let name = word.rsplit('/').next()?;
    (!name.is_empty() && !name.contains('$')).then(|| file.parent().unwrap_or(root).join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> PathBuf {
        let d = std::env::temp_dir().join(format!("xtask-sysroot-need-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("host/tests/wy")).unwrap();
        std::fs::create_dir_all(d.join("host/tests/mcp")).unwrap();
        d
    }

    #[test]
    fn image_scripts_and_their_libraries() {
        let d = tree();
        let w = |p: &str, t: &str| std::fs::write(d.join(p), t).unwrap();
        w("host/tests/wy/1-boot.sh", "cargo xtask image --name x\ncargo xtask qemu --name x\n");
        w("host/tests/wy/2-host.sh", "cargo test -p bitscomp\n");
        w("host/tests/mcp/lib.bash", "boot() { cargo xtask qemu --name \"$1\"; }\n");
        w("host/tests/wy/3-lib.sh", ". \"$ROOT/host/tests/mcp/lib.bash\"\nboot a\n");
        w("host/tests/wy/lib.bash", "build() { cargo xtask image \"$@\"; }\n");
        w("host/tests/wy/4-here.sh", ". \"$(dirname \"$0\")/lib.bash\"\nbuild\n");
        w("host/tests/wy/5-none.sh", ". \"$HERE/missing.bash\"\necho\n");
        w("host/tests/wy/6-cc.sh", "[ -f \"${V2_SYSROOT:-x}/usr/lib/libc.a\" ] || { echo SKIP; exit 0; }\n");
        let n = |p: &str| needs_sysroot(&d, &d.join(p));
        assert!(n("host/tests/wy/1-boot.sh"));
        assert!(!n("host/tests/wy/2-host.sh"));
        assert!(n("host/tests/wy/3-lib.sh"), "through a library below host/tests");
        assert!(n("host/tests/wy/4-here.sh"), "through a library next to the script");
        assert!(!n("host/tests/wy/5-none.sh"));
        assert!(n("host/tests/wy/6-cc.sh"), "builds against the sysroot");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The gate's V2 boots are recognised (directly and through mcp/lib.bash), the host-only scripts not.
    #[test]
    fn the_real_scripts() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for s in ["rb/56-update", "bt/76-confirm", "in/50-boot-screen", "gx/10-boot"] {
            assert!(needs_sysroot(&root, &root.join(format!("host/tests/{s}.sh"))), "{s}");
        }
        for s in ["sx/40-steam-launcher", "us/90-a11y-launchers", "gm/30-drmtrace-tools"] {
            assert!(!needs_sysroot(&root, &root.join(format!("host/tests/{s}.sh"))), "{s}");
        }
        // Builds against the sysroot without booting V2 (prints SKIP and passes without it).
        for s in ["ul/20-busybox-linux", "freeze/10-freeze", "dr/20-radv"] {
            assert!(needs_sysroot(&root, &root.join(format!("host/tests/{s}.sh"))), "{s}");
        }
    }
}
