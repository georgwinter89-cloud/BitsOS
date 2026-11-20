//! bitspkg's progress items as the `progress` lines of `do` (plan 1.10: download bytes, verify, unpack,
//! triggers, switch), in the order the run passes them; the final line (`done` or the error) is `ops`'s.

use bitspkg::progress::Item;
use settingsd::json::Json;

/// Download bytes so far (from `pkg.download.done`/`.cached`) against the plan's total.
#[derive(Default)]
pub struct Bytes {
    pub done: u64,
    pub total: u64,
}

fn kv_json(mut j: Json, kv: &[(String, String)]) -> Json {
    for (k, v) in kv {
        j = match v.parse::<i64>() {
            Ok(n) if k == "bytes" || k == "files" || k == "found" || k == "packages" => j.with(k, n),
            _ => j.with(k, v.as_str()),
        };
    }
    j
}

pub fn line(i: &Item, b: &mut Bytes) -> Json {
    match i {
        Item::Phase(p) => {
            let (phase, step) = match p.as_str() {
                "after download" => ("download", "done"),
                "after verify" => ("verify", "done"),
                "after unpack" => ("unpack", "done"),
                "after triggers" => ("triggers", "done"),
                "before switch" => ("switch", "start"),
                "after switch" => ("switch", "done"),
                other => (other, ""),
            };
            Json::obj().with("progress", phase).with("step", step)
        }
        Item::Event(name, kv) => {
            let phase = match name.as_str() {
                n if n.starts_with("pkg.download.") || n.starts_with("pkg.cache.") => "download",
                n if n.starts_with("pkg.verify.") => "verify",
                n if n.starts_with("pkg.unpack.") => "unpack",
                n if n.starts_with("pkg.trigger.") || n.starts_with("pkg.hook.") => "triggers",
                _ => "event",
            };
            let mut j = Json::obj().with("progress", phase).with("event", name.as_str());
            if name == "pkg.download.done" {
                let n = kv.iter().find(|(k, _)| k == "bytes").and_then(|(_, v)| v.parse::<u64>().ok()).unwrap_or(0);
                b.done = b.done.saturating_add(n);
                j = j.with("done_bytes", b.done).with("total_bytes", b.total);
            }
            kv_json(j, kv)
        }
        Item::Line(t) => Json::obj().with("progress", "report").with("text", t.as_str()),
    }
}

/// The phase order a run's lines keep (tests, and the page's bar).
pub fn rank(phase: &str) -> Option<usize> {
    ["download", "verify", "unpack", "triggers", "switch"].iter().position(|p| *p == phase)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_and_bytes() {
        let mut b = Bytes { done: 0, total: 300 };
        let d = line(&Item::Event("pkg.download.done".into(), vec![("pkg".into(), "jq".into()), ("bytes".into(), "200".into())]), &mut b);
        assert_eq!(d.str("progress"), Some("download"));
        assert_eq!(d.get("done_bytes").and_then(Json::as_i64), Some(200));
        assert_eq!(d.get("bytes").and_then(Json::as_i64), Some(200));
        assert_eq!(line(&Item::Phase("after triggers".into()), &mut b).str("progress"), Some("triggers"));
        assert_eq!(line(&Item::Phase("before switch".into()), &mut b).str("step"), Some("start"));
        assert_eq!(line(&Item::Line("Platz: …".into()), &mut b).str("progress"), Some("report"));
        assert!(rank("download") < rank("verify") && rank("unpack") < rank("switch"));
    }
}
