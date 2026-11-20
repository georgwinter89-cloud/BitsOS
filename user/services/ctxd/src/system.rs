//! CX-5: ctxd as BitsOS V2's service (`ctxd serve --system`, started by init as uid 1000, never root).
//! Its folder is `/data/appdata/ctxd` (0700, DESIGN §4), its socket `$XDG_RUNTIME_DIR/ctxd.sock` (0600; only
//! this user's programs: the desktop, the assistant, the CLI), its settings the `context.*` keys of
//! settingsd, its model one file in `models/` (not in the initramfs: the desk or a package copies it, see
//! `install_model`), llama-server the program `ctxd-llama` beside ctxd. No MCP over TCP: the assistant reaches
//! the tools through the socket (`sock.rs`, op `mcp`). A settings change, or the model arriving, starts the
//! service anew in the same process (`restart`), so every part sees one consistent configuration.

use crate::config::{Config, Context};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DATA: &str = "/data/appdata/ctxd";
pub const MODEL_FILE: &str = "multilingual-e5-small-q8_0.gguf";
/// BLAKE3 of the one model file CX-1 measured (TwinSunsLLC multilingual-e5-small q8_0, 132 439 008 bytes,
/// SHA256 e011debc1208e31bf7b6aebee2d9fc8bd2ca11694a77ed66ac9d0c9d0a877c93 as DESIGN.md 14.1 lists).
pub const MODEL_BLAKE3: &str = "7b7ad6bc9bfc588ad895179cb9e3069f80d89a93bd412465ed46d328bbc0fa12";
pub const MODEL_BYTES: u64 = 132_439_008;
/// How often the settings are looked at; while settingsd does not answer (init starts both at once) sooner.
const LOOK: Duration = Duration::from_secs(5);
const LOOK_SOON: Duration = Duration::from_secs(1);

pub fn model_path(data: &Path) -> PathBuf {
    data.join("models").join(MODEL_FILE)
}

/// llama-server as V2 ships it: `/bin/ctxd-llama`, beside ctxd itself.
fn llama_beside_me() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("ctxd-llama")))
        .unwrap_or_else(|| PathBuf::from("/bin/ctxd-llama"))
}

fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(r) => PathBuf::from(r),
        None => PathBuf::from(format!("/run/user/{}", usys::geteuid().unwrap_or(1000))),
    }
}

/// The service's configuration before settingsd is asked: no folder (nothing is indexed until Georg names
/// one), Background class for indexing, half the cores for the model, no TCP door.
pub fn base(data: &Path) -> Context {
    let mut c = Config::default().context;
    c.data = Some(data.to_path_buf());
    c.socket = Some(runtime_dir().join("ctxd.sock"));
    c.embedder.model = Some(model_path(data));
    c.embedder.server = Some(llama_beside_me());
    c.embedder.threads = std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).max(1));
    c.index.threads = 2;
    c.index.background = true;
    c.index.rescan_seconds = 600;
    c.mcp.enabled = false;
    // R8a on V2: the stick (/boot), the packages and the kernel's own trees are never document folders.
    c.forbidden_roots.extend(["/boot", "/apps", "/proc", "/sys", "/dev", "/run"].map(PathBuf::from));
    c
}

/// The model counts as there only at its full size: a copy still being written is not, so the end of the
/// copy changes the fingerprint and starts ctxd anew (once, not at every look while it grows).
pub fn model_ready(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && (p.file_name() != Some(MODEL_FILE.as_ref()) || m.len() == MODEL_BYTES))
}

/// What is running: the context from the settings plus whether the model file is there. A difference
/// between two looks means "start anew".
pub fn fingerprint(c: &Context) -> String {
    let model = c.embedder.model.as_deref().is_some_and(model_ready);
    format!("{}|model={model}", serde_json::to_string(c).unwrap_or_default())
}

/// Never root (it reads the owner's files with the owner's rights, and root could read every user's).
pub fn refuse_root() -> Result<(), String> {
    match usys::geteuid() {
        Ok(0) => Err("Der Kontextdienst läuft nie als root; init startet ihn als Benutzer 1000.".into()),
        _ => Ok(()),
    }
}

/// /data is mounted a moment after the session starts: wait for `/data/appdata` (made by the kernel for
/// uid 1000) instead of ending, and say so once. Its owner counts, not its being there: on a boot without
/// /data a root service may make the folder in the root file system, where ctxd may not write (it would end
/// and be restarted by init every few seconds for the whole boot).
pub fn wait_for_data(data: &Path) {
    let parent = data.parent().unwrap_or(Path::new("/"));
    let me = usys::geteuid().ok().map(|u| u as u32);
    let mut said = false;
    while !data_ready(parent, me) {
        if !said {
            eprintln!("ctxd: {} fehlt noch (wartet auf /data)", parent.display());
            said = true;
        }
        std::thread::sleep(LOOK);
    }
}

/// `parent` is a folder this user owns (or the uid is unknown and it is there).
fn data_ready(parent: &Path, me: Option<u32>) -> bool {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(parent) {
        Ok(m) if m.is_dir() => me.is_none_or(|u| m.uid() == u),
        _ => false,
    }
}

/// The current settings applied to `base`; Err says why only the base holds (logged by the caller).
pub fn configured(base: &Context, sock: &Path) -> (Context, Option<String>) {
    let mut c = base.clone();
    match crate::settings::fetch(sock) {
        Ok(v) => {
            crate::settings::apply(&mut c, &v);
            (c, None)
        }
        Err(e) => (c, Some(e)),
    }
}

/// Watches the settings and the model file; on a change ctxd starts anew (same pid, so init keeps it).
pub fn watch(base: Context, sock: PathBuf, running: String) {
    // The first look comes soon: at the start settingsd may not have listened yet (then the keys are not
    // registered, and Georg could not name a folder).
    let mut last_err = Some(String::new());
    loop {
        std::thread::sleep(if last_err.is_some() { LOOK_SOON } else { LOOK });
        let (c, err) = configured(&base, &sock);
        if err.is_some() && err != last_err && last_err.as_deref() != Some("") {
            crate::event!("warn", "settings.unavailable", reason = err.clone().unwrap_or_default());
        }
        last_err = err;
        let now = fingerprint(&c);
        if now != running {
            crate::event!("info", "service.restart", reason = "settings_or_model_changed");
            restart();
        }
    }
}

/// exec of the same program with the same arguments. Every descriptor is close-on-exec, so the socket, the
/// llama-server pipe (its wrapper ends it) and the workers' pipes close; the new run binds the socket anew.
pub fn restart() -> ! {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/bin/ctxd"));
    let e = std::process::Command::new(&exe).args(std::env::args_os().skip(1)).exec();
    eprintln!("ctxd: Neustart misslungen ({e}); init startet den Dienst neu");
    std::process::exit(1)
}

/// `ctxd model-install FILE`: checks the file is the model CX-1 measured and copies it into
/// `<data>/models/` (temp + rename). The desk copies one file this way; a package can do the same later.
pub fn install_model(file: &Path, data: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let len = f.metadata().map_err(|e| e.to_string())?.len();
    if len != MODEL_BYTES {
        return Err(format!("{}: {len} Bytes, das Modell hat {MODEL_BYTES}", file.display()));
    }
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    let got = h.finalize().to_hex().to_string();
    if got != MODEL_BLAKE3 {
        return Err(format!("{}: Prüfsumme {got} passt nicht zum Modell ({MODEL_BLAKE3})", file.display()));
    }
    crate::util::private_dir(data).map_err(|e| format!("{}: {e}", data.display()))?;
    let dir = data.join("models");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join(format!(".{MODEL_FILE}.tmp"));
    std::fs::copy(file, &tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::File::open(&tmp).and_then(|t| t.sync_all()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, model_path(data)).map_err(|e| e.to_string())?;
    Ok(format!("Modell eingesetzt: {}", model_path(data).display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_service_starts_with_no_folder_no_tcp_and_in_the_background() {
        let c = base(Path::new("/data/appdata/ctxd"));
        assert!(c.roots.is_empty());
        assert!(!c.mcp.enabled);
        assert!(c.index.background);
        assert_eq!(c.embedder.model.as_deref(), Some(Path::new("/data/appdata/ctxd/models/multilingual-e5-small-q8_0.gguf")));
        assert!(c.forbidden_roots.contains(&PathBuf::from("/boot")));
    }

    #[test]
    fn data_is_ready_only_in_a_folder_of_our_own() {
        use std::os::unix::fs::MetadataExt;
        let d = std::env::temp_dir().join(format!("ctxd-own-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        assert!(!data_ready(&d, None), "missing");
        std::fs::create_dir_all(&d).unwrap();
        let mine = std::fs::metadata(&d).unwrap().uid();
        assert!(data_ready(&d, Some(mine)));
        assert!(!data_ready(&d, Some(mine.wrapping_add(1))), "another user's folder (root's on a boot without /data)");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_model_still_being_copied_is_no_model() {
        let d = std::env::temp_dir().join(format!("ctxd-fp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let c = base(&d);
        let absent = fingerprint(&c);
        std::fs::create_dir_all(d.join("models")).unwrap();
        let f = std::fs::File::create(model_path(&d)).unwrap();
        f.set_len(MODEL_BYTES / 3).unwrap();
        assert_eq!(fingerprint(&c), absent, "a third of the model");
        f.set_len(MODEL_BYTES / 2).unwrap();
        assert_eq!(fingerprint(&c), absent, "half: still no restart");
        f.set_len(MODEL_BYTES).unwrap();
        assert_ne!(fingerprint(&c), absent, "the whole model: start anew");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_wrong_file_is_not_taken_as_the_model() {
        let d = std::env::temp_dir().join(format!("ctxd-model-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("x.gguf");
        std::fs::write(&f, b"not a model").unwrap();
        assert!(install_model(&f, &d.join("data")).unwrap_err().contains("Bytes"));
        assert!(!model_path(&d.join("data")).exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
