//! bitscomp and the Mesa tree it is linked against (build.rs: the tree's loader and DT_RPATH, `\BITSOS2\mesa\` on the
//! stick). glvnd and Mesa find the tree's files through environment variables set for bitscomp and Xwayland only
//! (`set_env`); the programs bitscomp starts never get them (`GLES_ENV`, removed in `App::spawn`). The shader cache is
//! on and lies in the desktop user's home on the data volume (`set_shader_cache`), never on the stick.
//!
//! A machine without a render node that opens (no GPU driver yet, a lost device, QEMU) has no desktop: bitscomp says so
//! in one line and ends before it touches the screen (`gpu_or_why`), so the console keeps it.

use std::path::{Path, PathBuf};

/// The variables `set_env` and `set_shader_cache` set and that no program bitscomp starts may inherit: they name the
/// tree, which only bitscomp's and Xwayland's glibc may load.
pub const GLES_ENV: &[&str] = &["__EGL_VENDOR_LIBRARY_FILENAMES", "LIBGL_DRIVERS_PATH", "GBM_BACKENDS_PATH", "DRIRC_CONFIGDIR", SHADER_CACHE];
/// Mesa's own variable: its cache folder (`mesa_shader_cache_db` below it, util/disk_cache_os.c).
pub const SHADER_CACHE: &str = "MESA_SHADER_CACHE_DIR";
/// The tree's lib/ (build.rs): its loader, the DT_RPATH, its EGL.
const TREE_LIB: &str = env!("BITSCOMP_GPU_LIB");

/// The tree bitscomp was linked for (the parent of its lib/).
pub fn tree_root() -> &'static str {
    let lib = TREE_LIB.trim_end_matches('/');
    lib.rsplit_once('/').map_or(lib, |(root, _)| root)
}

/// The tree's variables: the EGL vendor file, the DRI and GBM drivers, the driver defaults.
pub fn env_values(tree: &str) -> Vec<(&'static str, String)> {
    let tree = tree.trim_end_matches('/');
    let mut v = vec![
        ("__EGL_VENDOR_LIBRARY_FILENAMES", format!("{tree}/share/glvnd/egl_vendor.d/50_mesa.json")),
        ("LIBGL_DRIVERS_PATH", format!("{tree}/lib/dri")),
        ("GBM_BACKENDS_PATH", format!("{tree}/lib/gbm")),
        ("DRIRC_CONFIGDIR", format!("{tree}/share/drirc.d")),
    ];
    // Xwayland's glamor caches its shaders in the same place.
    if let Ok(dir) = std::env::var(SHADER_CACHE) {
        v.push((SHADER_CACHE, dir));
    }
    v
}

/// The tree's variables for bitscomp's own EGL. Called from `main` before any thread exists.
pub fn set_env() {
    for (k, v) in env_values(tree_root()) {
        // SAFETY: called from `main` before `run` starts any thread, so nothing reads the environment concurrently.
        unsafe { std::env::set_var(k, v) };
    }
}

/// The shader cache's folder for the desktop user whose home is `home`: `<home>/.cache` (Mesa adds its own folder).
pub fn shader_cache_dir(home: &Path) -> PathBuf {
    home.join(".cache")
}

/// The shader cache on, in `home` (made 0700 when missing). Called while bitscomp has one thread, as the desktop
/// user, before the first EGL display. Mesa is always pointed at this folder, never left to its own default (which
/// follows HOME and could lie elsewhere): a folder that cannot be made means Mesa cannot open its cache there and
/// draws uncached; the line says so.
pub fn set_shader_cache(home: &Path) {
    use std::os::unix::fs::DirBuilderExt;
    let dir = shader_cache_dir(home);
    if let Err(e) = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir) {
        eprintln!("bitscomp: shader cache {}: {e}: shaders are compiled at every start", dir.display());
    }
    // SAFETY: called from `run` before the event loop, session, settings or MCP threads exist (as `user_env`).
    unsafe { std::env::set_var(SHADER_CACHE, &dir) };
}

/// The render node to draw on, or the one line why this machine has no desktop: none there, or a lost one (`look`: a
/// lost device keeps its node and answers a new open with ENODEV).
pub fn gpu_or_why(node: Option<&str>, look: impl FnOnce(&str) -> crate::gpu_watch::Look) -> Result<String, String> {
    let Some(node) = node else {
        return Err("no GPU render node (/dev/dri/renderD*): no desktop on this machine, the text console stays".into());
    };
    match look(node) {
        crate::gpu_watch::Look::Gone(e) => Err(format!("the GPU render node {node} is lost ({e}): no desktop, the text console stays")),
        // Anything else says nothing about the device: EGL's own open tells.
        crate::gpu_watch::Look::Opens | crate::gpu_watch::Look::Other => Ok(node.to_string()),
    }
}

/// Whether `libEGL.so.1` loads (through the DT_RPATH, the way Smithay's libloading will open it), then unloaded again.
/// Nothing of EGL is called: Smithay's EGL loader panics when the library does not load (egl/ffi.rs:148).
pub fn egl_probe() -> String {
    probe("libEGL.so.1")
}

fn probe(soname: &str) -> String {
    let Ok(name) = std::ffi::CString::new(soname) else { return "bad-name".into() };
    // SAFETY: dlopen with a NUL-terminated name; the handle is only passed back to dlclose, and dlerror's text is
    // copied before the next dl call. Loading runs the library's constructors (glvnd's: none that touch a display).
    unsafe {
        let h = libc::dlopen(name.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
        if h.is_null() {
            let err = libc::dlerror();
            let why = if err.is_null() { "unknown".into() } else { std::ffi::CStr::from_ptr(err).to_string_lossy().into_owned() };
            eprintln!("bitscomp: {soname} does not load: {why}");
            return "missing".into();
        }
        libc::dlclose(h);
    }
    "loadable".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu_watch::Look;

    #[test]
    fn the_names_are_the_ones_children_lose() {
        let vals = env_values("/t/");
        let mut names: Vec<&str> = vals.iter().map(|(k, _)| *k).collect();
        if !names.contains(&SHADER_CACHE) {
            names.push(SHADER_CACHE);
        }
        assert_eq!(names, GLES_ENV);
        assert_eq!(vals[0].1, "/t/share/glvnd/egl_vendor.d/50_mesa.json");
        assert!(!GLES_ENV.contains(&"MESA_SHADER_CACHE_DISABLE") && !GLES_ENV.contains(&"LIBGL_ALWAYS_SOFTWARE"));
    }

    #[test]
    fn the_tree_is_the_parent_of_the_linked_lib() {
        assert_eq!(format!("{}/lib", tree_root()), TREE_LIB.trim_end_matches('/'));
    }

    #[test]
    fn the_shader_cache_lies_in_the_users_home() {
        assert_eq!(shader_cache_dir(Path::new("/data/home/1000")), Path::new("/data/home/1000/.cache"));
        let home = std::env::temp_dir().join(format!("bitscomp-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let dir = shader_cache_dir(&home);
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn without_a_node_that_opens_bitscomp_ends_with_one_line() {
        let none = gpu_or_why(None, |_| panic!("nothing to look at")).unwrap_err();
        assert!(none.contains("no GPU render node") && none.contains("text console") && !none.contains('\n'), "{none}");
        let lost = gpu_or_why(Some("/dev/dri/renderD128"), |_| Look::Gone("No such device".into())).unwrap_err();
        assert!(lost.contains("/dev/dri/renderD128 is lost (No such device)") && !lost.contains('\n'), "{lost}");
        assert_eq!(gpu_or_why(Some("/dev/dri/renderD129"), |_| Look::Opens), Ok("/dev/dri/renderD129".to_string()));
        assert_eq!(gpu_or_why(Some("/dev/dri/renderD128"), |_| Look::Other), Ok("/dev/dri/renderD128".to_string()), "EBUSY says nothing");
    }
}
