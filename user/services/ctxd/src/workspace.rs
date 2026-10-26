//! Workspaces (DESIGN §2.2, §4): one JSON file per workspace (temp + rename), every edit with its inverse
//! in `journal.jsonl` (B1). Items refer to document ids, which stay stable across renames.

use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Item {
    pub doc: String,
    /// "search" (found by the workspace's query) or "manual" (added by Georg).
    pub how: String,
    pub added: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    pub query: Option<String>,
    pub pinned: bool,
    pub created: i64,
    pub last_used: i64,
    pub items: Vec<Item>,
    #[serde(default)]
    pub dismissed: Vec<String>,
}

pub struct WorkspaceStore {
    dir: PathBuf,
    lock: Mutex<()>,
}

impl WorkspaceStore {
    pub fn open(dir: &Path) -> std::io::Result<WorkspaceStore> {
        fs::create_dir_all(dir)?;
        let st = WorkspaceStore { dir: dir.to_path_buf(), lock: Mutex::new(()) };
        st.check_journal();
        Ok(st)
    }

    /// A torn last journal line (crash while appending) is dropped and logged, never fatal (§4.3).
    fn check_journal(&self) {
        let p = self.dir.join("journal.jsonl");
        let Ok(b) = fs::read(&p) else { return };
        if b.is_empty() {
            return;
        }
        let mut good = 0;
        let mut pos = 0;
        for line in b.split_inclusive(|c| *c == b'\n') {
            if line.ends_with(b"\n") && serde_json::from_slice::<serde_json::Value>(line).is_ok() {
                good = pos + line.len();
            } else {
                break;
            }
            pos += line.len();
        }
        if good < b.len() {
            crate::event!(
                "warn",
                "recover.torn_tail",
                file = "workspaces/journal.jsonl",
                dropped_bytes = b.len() - good
            );
            let _ = crate::util::write_atomic(&p, &b[..good], true);
        }
    }

    fn path(&self, id: &str) -> Option<PathBuf> {
        // Ids are ours ("ws_" + base32); anything else is refused before it touches a path.
        (id.starts_with("ws_") && id.len() == 29 && id[3..].bytes().all(|c| c.is_ascii_alphanumeric()))
            .then(|| self.dir.join(format!("{id}.json")))
    }

    pub fn get(&self, id: &str) -> Option<Workspace> {
        let b = fs::read(self.path(id)?).ok()?;
        serde_json::from_slice(&b).ok()
    }

    pub fn list(&self) -> Vec<Workspace> {
        let mut v: Vec<Workspace> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
            .filter_map(|e| serde_json::from_slice(&fs::read(e.path()).ok()?).ok())
            .collect();
        v.sort_by(|a: &Workspace, b: &Workspace| b.pinned.cmp(&a.pinned).then(b.last_used.cmp(&a.last_used)));
        v
    }

    pub fn find_by_name(&self, name: &str) -> Option<Workspace> {
        let n = name.trim().to_lowercase();
        self.list().into_iter().find(|w| w.name.to_lowercase() == n)
    }

    /// Save with a journal entry holding the state before (`null` for a new workspace). Callers that edit an
    /// existing workspace use `transact`, so the read and the write are one step.
    pub fn save(&self, ws: &Workspace, op: &str) -> std::io::Result<()> {
        self.transact(|t| t.save(ws, op))?
    }

    /// Run `f` with the store locked: a read-modify-write inside it cannot interleave with another one, so two
    /// connections editing the same workspace never lose an edit and the journal's `before` is the true state.
    /// Slow work (a search) belongs before the call; only the merge runs under the lock.
    pub fn transact<T>(&self, f: impl FnOnce(&Txn<'_>) -> T) -> std::io::Result<T> {
        let _g = self.lock.lock().map_err(|_| std::io::Error::other("poisoned"))?;
        Ok(f(&Txn { st: self }))
    }

    fn save_locked(&self, ws: &Workspace, op: &str) -> std::io::Result<()> {
        let path = self.path(&ws.id).ok_or_else(|| std::io::Error::other("bad id"))?;
        let before = fs::read(&path).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
        self.journal(
            serde_json::json!({ "ts": crate::util::now_secs(), "op": op, "workspace": ws.id, "before": before }),
        )?;
        crate::util::write_atomic(&path, &serde_json::to_vec_pretty(ws).map_err(std::io::Error::other)?, true)
    }

    pub fn delete(&self, id: &str) -> std::io::Result<bool> {
        let _g = self.lock.lock().map_err(|_| std::io::Error::other("poisoned"))?;
        let Some(path) = self.path(id) else { return Ok(false) };
        if !path.exists() {
            return Ok(false);
        }
        // Forgetting means forgetting: the journal keeps only that it happened, not the content.
        self.journal(serde_json::json!({ "ts": crate::util::now_secs(), "op": "forget", "workspace": id }))?;
        fs::remove_file(path)?;
        Ok(true)
    }

    fn journal(&self, v: serde_json::Value) -> std::io::Result<()> {
        let mut f = OpenOptions::new().create(true).append(true).open(self.dir.join("journal.jsonl"))?;
        f.write_all(format!("{v}\n").as_bytes())?;
        f.sync_data()
    }

    /// Remove a document from every workspace (forget scope `document`).
    pub fn remove_doc_everywhere(&self, doc: &str) -> usize {
        self.transact(|t| {
            let mut n = 0;
            for mut w in t.list() {
                let before = w.items.len() + w.dismissed.len();
                w.items.retain(|i| i.doc != doc);
                w.dismissed.retain(|d| d != doc);
                if w.items.len() + w.dismissed.len() != before {
                    n += 1;
                    let _ = t.save(&w, "forget_document");
                }
            }
            n
        })
        .unwrap_or(0)
    }

    pub fn wipe(&self) -> usize {
        let _g = self.lock.lock();
        let mut n = 0;
        for e in fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            if fs::remove_file(e.path()).is_ok() {
                n += 1;
            }
        }
        n
    }

    pub fn journal_lines(&self) -> usize {
        fs::File::open(self.dir.join("journal.jsonl")).map(|f| std::io::BufReader::new(f).lines().count()).unwrap_or(0)
    }
}

/// The store while its lock is held (see `WorkspaceStore::transact`).
pub struct Txn<'a> {
    st: &'a WorkspaceStore,
}

impl Txn<'_> {
    pub fn get(&self, id: &str) -> Option<Workspace> {
        self.st.get(id)
    }

    pub fn list(&self) -> Vec<Workspace> {
        self.st.list()
    }

    pub fn find_by_name(&self, name: &str) -> Option<Workspace> {
        self.st.find_by_name(name)
    }

    pub fn save(&self, ws: &Workspace, op: &str) -> std::io::Result<()> {
        self.st.save_locked(ws, op)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_list_and_torn_journal() {
        let d = std::env::temp_dir().join(format!("ctxd-ws-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let st = WorkspaceStore::open(&d).unwrap();
        let w = Workspace {
            id: crate::util::new_id("ws"),
            name: "Steuer".into(),
            query: None,
            pinned: false,
            created: 1,
            last_used: 1,
            items: vec![],
            dismissed: vec![],
        };
        st.save(&w, "create").unwrap();
        assert_eq!(st.find_by_name("steuer").unwrap().id, w.id);
        assert!(st.get("../../etc/passwd").is_none());
        // A crash in the middle of an append.
        let mut f = OpenOptions::new().append(true).open(d.join("journal.jsonl")).unwrap();
        f.write_all(b"{\"ts\":1,\"op\":\"x\"").unwrap();
        drop(f);
        let st = WorkspaceStore::open(&d).unwrap();
        assert_eq!(st.journal_lines(), 1);
        let _ = fs::remove_dir_all(&d);
    }
}
