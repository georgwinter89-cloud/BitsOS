//! The private workshop repository's releases through GitHub's REST API (plan/WERKSTATT.md): `version-<N>`
//! releases, their assets fetched through the API (`Accept: application/octet-stream`, answered with a redirect
//! to GitHub's storage host; fetch drops the token on that redirect). Pure: URLs and the answer's parsing.

use mcp_wire::json::Json;

pub const API: &str = "https://api.github.com";
pub const REPO: &str = "georgwinter89-cloud/bitsos-werkstatt";
pub const TAG_PREFIX: &str = "version-";
pub const JSON_ACCEPT: &str = "application/vnd.github+json";
pub const ASSET_ACCEPT: &str = "application/octet-stream";
/// The release list's answer is read up to this (30 releases with their texts).
pub const LIST_MAX: u64 = 4 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub id: u64,
    pub name: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: u32,
    pub tag: String,
    pub published: String,
    pub assets: Vec<Asset>,
}

impl Release {
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

pub fn list_url(api: &str, repo: &str) -> String {
    format!("{}/repos/{repo}/releases?per_page=30", api.trim_end_matches('/'))
}

pub fn asset_url(api: &str, repo: &str, id: u64) -> String {
    format!("{}/repos/{repo}/releases/assets/{id}", api.trim_end_matches('/'))
}

/// The number of a `version-<N>` tag.
pub fn tag_version(tag: &str) -> Option<u32> {
    let d = tag.strip_prefix(TAG_PREFIX)?;
    (!d.is_empty() && d.len() <= 8 && d.bytes().all(|b| b.is_ascii_digit())).then(|| d.parse().ok()).flatten().filter(|&n| n > 0)
}

/// The newest published `version-<N>` release of the list (drafts and other tags left out).
pub fn newest(list: &[u8]) -> Result<Option<Release>, String> {
    let t = std::str::from_utf8(list).map_err(|_| String::from("GitHub answered no text"))?;
    let j = Json::parse(t).map_err(|e| format!("GitHub answered no JSON (at byte {})", e.at))?;
    let Some(arr) = j.as_array() else {
        let msg = j.get("message").and_then(Json::as_str).unwrap_or("no list");
        return Err(format!("GitHub answered: {msg}"));
    };
    let mut best: Option<Release> = None;
    for r in arr {
        if r.get("draft").and_then(Json::as_bool) == Some(true) {
            continue;
        }
        let Some(tag) = r.get("tag_name").and_then(Json::as_str) else { continue };
        let Some(version) = tag_version(tag) else { continue };
        if best.as_ref().is_some_and(|b| b.version >= version) {
            continue;
        }
        let assets = r
            .get("assets")
            .and_then(Json::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| {
                        Some(Asset { id: x.get("id")?.as_u64()?, name: x.get("name")?.as_str()?.to_string(), size: x.get("size")?.as_u64()? })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let published = r.get("published_at").and_then(Json::as_str).unwrap_or("").chars().take(10).collect();
        best = Some(Release { version, tag: tag.to_string(), published, assets });
    }
    Ok(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_version_release() {
        let list = br#"[
          {"tag_name":"version-165","draft":false,"published_at":"2026-11-16T10:00:00Z","assets":[{"id":11,"name":"KERNEL","size":100,"url":"x"},{"id":12,"name":"MANIFEST","size":200}]},
          {"tag_name":"version-170","draft":true,"assets":[]},
          {"tag_name":"v2026-11-11","draft":false,"assets":[]},
          {"tag_name":"version-164","draft":false,"assets":[]}
        ]"#;
        let r = newest(list).unwrap().unwrap();
        assert_eq!((r.version, r.tag.as_str(), r.published.as_str()), (165, "version-165", "2026-11-16"));
        assert_eq!(r.asset("MANIFEST"), Some(&Asset { id: 12, name: "MANIFEST".into(), size: 200 }));
        assert_eq!(newest(b"[]").unwrap(), None);
        assert_eq!(newest(br#"{"message":"Bad credentials"}"#).unwrap_err(), "GitHub answered: Bad credentials");
        assert!(newest(b"<html>").is_err());
        for (t, v) in [("version-1", Some(1)), ("version-0", None), ("version-", None), ("version-12a", None), ("version-123456789", None), ("Version-5", None)] {
            assert_eq!(tag_version(t), v, "{t}");
        }
        assert_eq!(list_url("https://api.github.com/", REPO), "https://api.github.com/repos/georgwinter89-cloud/bitsos-werkstatt/releases?per_page=30");
        assert_eq!(asset_url(API, "o/r", 7), "https://api.github.com/repos/o/r/releases/assets/7");
    }
}
