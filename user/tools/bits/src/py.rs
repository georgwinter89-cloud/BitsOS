//! The Python module `bitsos` (user/tools/bits/python), carried inside bits so it always matches it: written once
//! per version into the runtime folder and put on the interpreter's path by the command view.

use std::path::PathBuf;

pub const FILES: &[(&str, &str)] = &[
    ("__init__.py", include_str!("../python/bitsos/__init__.py")),
    ("_bits.py", include_str!("../python/bitsos/_bits.py")),
    ("fenster.py", include_str!("../python/bitsos/fenster.py")),
    ("ui.py", include_str!("../python/bitsos/ui.py")),
    ("einstellungen.py", include_str!("../python/bitsos/einstellungen.py")),
    ("arbeitsraum.py", include_str!("../python/bitsos/arbeitsraum.py")),
    ("dateien.py", include_str!("../python/bitsos/dateien.py")),
    ("zwischenablage.py", include_str!("../python/bitsos/zwischenablage.py")),
    ("foto.py", include_str!("../python/bitsos/foto.py")),
    ("meldung.py", include_str!("../python/bitsos/meldung.py")),
    ("messwerte.py", include_str!("../python/bitsos/messwerte.py")),
    ("programm.py", include_str!("../python/bitsos/programm.py")),
    ("papierkorb.py", include_str!("../python/bitsos/papierkorb.py")),
];

fn fnv(parts: &[(&str, &str)]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for (n, t) in parts {
        for b in n.bytes().chain(t.bytes()) {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// The folder to put on PYTHONPATH (it holds `bitsos/`).
pub fn install() -> Result<PathBuf, String> {
    let base = crate::sock::runtime_dir().join("bits-python");
    let dir = base.join(format!("{:016x}", fnv(FILES)));
    if dir.join("bitsos").join("__init__.py").is_file() {
        return Ok(dir);
    }
    let tmp = base.join(format!(".new-{}", std::process::id()));
    let pkg = tmp.join("bitsos");
    std::fs::create_dir_all(&pkg).map_err(|e| format!("{}: {e}", pkg.display()))?;
    for (n, t) in FILES {
        std::fs::write(pkg.join(n), t).map_err(|e| format!("{n}: {e}"))?;
    }
    match std::fs::rename(&tmp, &dir) {
        Ok(()) => Ok(dir),
        // Another bits made it at the same moment: its copy is the same.
        Err(_) if dir.join("bitsos").join("__init__.py").is_file() => {
            let _ = std::fs::remove_dir_all(&tmp);
            Ok(dir)
        }
        Err(e) => Err(format!("{}: {e}", dir.display())),
    }
}
