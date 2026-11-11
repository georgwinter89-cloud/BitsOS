//! The release manifest (`manifest.json`): every asset of one release with its size and SHA-256. The manifest's
//! own hash is compiled into the installer (`release::EMBEDDED_MANIFEST_SHA256`), so a manifest someone changed is
//! refused before any asset is looked at, and an asset is used only when it matches the manifest.

use crate::json::{self, Value, quote};
use crate::sha;
use std::path::Path;

/// GitHub refuses release assets of 2 GiB and more.
pub const MAX_ASSET: u64 = (2 << 30) - 1;
pub const FILE: &str = "manifest.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// bitsboot, kernel, initramfs, firmware, `\BITSOS2\bin\`.
    System,
    /// One program tree `\BITSOS2\<tree>\` as it lands on the stick.
    Tree,
    /// The open-source part of a tree whose vendor files the installer fetches itself (Brave, Steam).
    Runtime,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::System => "system",
            Kind::Tree => "tree",
            Kind::Runtime => "runtime",
        }
    }
    fn parse(s: &str) -> Option<Kind> {
        [Kind::System, Kind::Tree, Kind::Runtime].into_iter().find(|k| k.as_str() == s)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    pub sha256: String,
    pub kind: Kind,
    /// The tree it fills (Tree, Runtime).
    pub tree: Option<String>,
    /// Trees it needs on the same stick (vkQuake and Xwayland run on the Mesa tree).
    pub needs: Vec<String>,
    /// Bytes and files once unpacked (the space check before anything is written).
    pub unpacked: u64,
    pub files: u64,
    /// A runtime's files that come from their maker (its `VENDOR.txt`), for the space check before any download.
    pub vendor_bytes: u64,
    pub vendor_files: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub version: String,
    pub assets: Vec<Asset>,
}

pub fn name_ok(n: &str) -> bool {
    !n.is_empty() && n.len() <= 128 && !n.starts_with('.') && n.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-+".contains(&b))
}

/// A tree folder name the installer will create below `\BITSOS2\`.
pub fn tree_ok(n: &str) -> bool {
    name_ok(n) && !["bin", "sys", "home", "mcp", "firmware", "licenses"].contains(&n.to_ascii_lowercase().as_str())
}

impl Manifest {
    pub fn to_json(&self) -> String {
        let mut s = format!("{{\n  \"format\": 1,\n  \"version\": {},\n  \"assets\": [\n", quote(&self.version));
        for (i, a) in self.assets.iter().enumerate() {
            let tree = a.tree.as_deref().map(|t| format!(", \"tree\": {}", quote(t))).unwrap_or_default();
            let vendor = if a.kind == Kind::Runtime { format!(", \"vendor_bytes\": {}, \"vendor_files\": {}", a.vendor_bytes, a.vendor_files) } else { String::new() };
            let needs: Vec<String> = a.needs.iter().map(|n| quote(n)).collect();
            s.push_str(&format!(
                "    {{\"name\": {}, \"size\": {}, \"sha256\": {}, \"kind\": {}{tree}, \"needs\": [{}], \"unpacked\": {}, \"files\": {}{vendor}}}{}\n",
                quote(&a.name),
                a.size,
                quote(&a.sha256),
                quote(a.kind.as_str()),
                needs.join(", "),
                a.unpacked,
                a.files,
                if i + 1 < self.assets.len() { "," } else { "" }
            ));
        }
        s.push_str("  ]\n}\n");
        s
    }

    pub fn parse(text: &str) -> Result<Manifest, String> {
        let v = json::parse(text).map_err(|e| format!("manifest: {e}"))?;
        if v.num("format")? != 1 {
            return Err("manifest: unknown format".into());
        }
        let version = v.str("version")?.to_string();
        if !name_ok(&version) {
            return Err(format!("manifest: bad version {version:?}"));
        }
        let Some(Value::Arr(list)) = v.get("assets") else { return Err("manifest: no asset list".into()) };
        let mut assets: Vec<Asset> = Vec::new();
        for a in list {
            let name = a.str("name")?.to_string();
            let sha256 = a.str("sha256")?.to_string();
            let kind = Kind::parse(a.str("kind")?).ok_or_else(|| format!("manifest: {name}: unknown kind"))?;
            let tree = match a.get("tree") {
                None => None,
                Some(Value::Str(t)) if tree_ok(t) => Some(t.clone()),
                Some(_) => return Err(format!("manifest: {name}: bad tree name")),
            };
            let needs = match a.get("needs") {
                None => Vec::new(),
                Some(Value::Arr(n)) => n.iter().map(|x| match x {
                    Value::Str(t) if tree_ok(t) => Ok(t.clone()),
                    _ => Err(format!("manifest: {name}: bad entry in needs")),
                }).collect::<Result<_, _>>()?,
                Some(_) => return Err(format!("manifest: {name}: needs is no list")),
            };
            let opt = |k: &str| if a.get(k).is_some() { a.num(k) } else { Ok(0) };
            let (vendor_bytes, vendor_files) = (opt("vendor_bytes")?, opt("vendor_files")?);
            let asset = Asset { size: a.num("size")?, name, sha256, kind, tree, needs, unpacked: a.num("unpacked")?, files: a.num("files")?, vendor_bytes, vendor_files };
            if !name_ok(&asset.name) || !asset.name.ends_with(".tar.zst") {
                return Err(format!("manifest: bad asset name {:?}", asset.name));
            }
            if !sha::is_hex64(&asset.sha256) {
                return Err(format!("manifest: {}: sha256 is not 64 hex digits", asset.name));
            }
            if asset.size > MAX_ASSET {
                return Err(format!("manifest: {}: larger than a release asset can be", asset.name));
            }
            if (asset.kind == Kind::System) != asset.tree.is_none() {
                return Err(format!("manifest: {}: a tree asset names its tree, the system names none", asset.name));
            }
            if assets.iter().any(|b| b.name == asset.name || (b.kind == asset.kind && b.tree == asset.tree)) {
                return Err(format!("manifest: {} is listed twice", asset.name));
            }
            assets.push(asset);
        }
        if assets.iter().filter(|a| a.kind == Kind::System).count() != 1 {
            return Err("manifest: exactly one system asset is needed".into());
        }
        Ok(Manifest { version, assets })
    }

    pub fn system(&self) -> &Asset {
        self.assets.iter().find(|a| a.kind == Kind::System).expect("checked in parse")
    }

    pub fn trees(&self) -> impl Iterator<Item = &Asset> {
        self.assets.iter().filter(|a| a.kind != Kind::System)
    }
}

/// The manifest, only if its bytes hash to `expected` (the hash compiled into this installer).
pub fn verify(bytes: &[u8], expected: &str) -> Result<Manifest, String> {
    let got = sha::hex(bytes);
    if got != expected {
        return Err(format!(
            "refused: the release manifest's SHA-256 is {got}, this installer expects {expected}: it was changed or belongs to another release. Nothing was written."
        ));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "manifest: not UTF-8".to_string())?;
    Manifest::parse(text)
}

/// The asset file at `path`, only if its size and SHA-256 are the manifest's.
pub fn verify_asset(path: &Path, a: &Asset) -> Result<(), String> {
    let len = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?.len();
    if len != a.size {
        return Err(format!("refused: {} has {len} bytes, the manifest says {}. Nothing was written.", a.name, a.size));
    }
    let (got, _) = sha::of_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if got != a.sha256 {
        return Err(format!("refused: {}'s SHA-256 is not the manifest's (a damaged or changed download). Nothing was written.", a.name));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn sample() -> Manifest {
        Manifest {
            version: "0.2.0".into(),
            assets: vec![
                Asset { name: "bitsos-system-0.2.0.tar.zst".into(), size: 3, sha256: sha::hex(b"sys"), kind: Kind::System, tree: None, needs: vec![], unpacked: 10, files: 2, vendor_bytes: 0, vendor_files: 0 },
                Asset { name: "bitsos-mesa-0.2.0.tar.zst".into(), size: 4, sha256: sha::hex(b"mesa"), kind: Kind::Tree, tree: Some("mesa".into()), needs: vec![], unpacked: 10, files: 2, vendor_bytes: 0, vendor_files: 0 },
                Asset { name: "bitsos-vkquake-0.2.0.tar.zst".into(), size: 2, sha256: sha::hex(b"vq"), kind: Kind::Tree, tree: Some("vkquake".into()), needs: vec!["mesa".into()], unpacked: 5, files: 1, vendor_bytes: 0, vendor_files: 0 },
                Asset { name: "bitsos-brave-runtime-0.2.0.tar.zst".into(), size: 2, sha256: sha::hex(b"br"), kind: Kind::Runtime, tree: Some("brave".into()), needs: vec![], unpacked: 5, files: 1, vendor_bytes: 400 << 20, vendor_files: 94 },
            ],
        }
    }

    #[test]
    fn round_trip() {
        let m = sample();
        assert_eq!(Manifest::parse(&m.to_json()).unwrap(), m);
    }

    #[test]
    fn good_manifest_and_assets_pass() {
        let d = std::env::temp_dir().join(format!("bi-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let text = sample().to_json();
        let m = verify(text.as_bytes(), &sha::hex(text.as_bytes())).unwrap();
        std::fs::write(d.join("s"), b"sys").unwrap();
        verify_asset(&d.join("s"), m.system()).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn tampered_manifest_is_refused() {
        let text = sample().to_json();
        let pinned = sha::hex(text.as_bytes());
        // Someone points the system asset at their own file and fixes its hash in the manifest.
        let evil = text.replace(&sha::hex(b"sys"), &sha::hex(b"evil"));
        let e = verify(evil.as_bytes(), &pinned).unwrap_err();
        assert!(e.contains("refused") && e.contains("changed"), "{e}");
    }

    #[test]
    fn tampered_asset_is_refused() {
        let d = std::env::temp_dir().join(format!("bi-asset-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let m = sample();
        std::fs::write(d.join("s"), b"syz").unwrap();
        assert!(verify_asset(&d.join("s"), m.system()).unwrap_err().contains("SHA-256"));
        std::fs::write(d.join("s"), b"sysx").unwrap();
        assert!(verify_asset(&d.join("s"), m.system()).unwrap_err().contains("bytes"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bad_entries_are_refused() {
        let ok = sample().to_json();
        for (from, to) in [("bitsos-mesa-0.2.0.tar.zst", "../x.tar.zst"), ("\"mesa\"", "\"SYS\""), ("\"size\": 4", "\"size\": 4, \"size\": 5"), ("\"size\": 4", "\"size\": 2147483648")] {
            assert!(Manifest::parse(&ok.replacen(from, to, 1)).is_err(), "{from} -> {to}");
        }
        let mut two = sample();
        two.assets.push(two.assets[1].clone());
        assert!(Manifest::parse(&two.to_json()).is_err());
    }
}
