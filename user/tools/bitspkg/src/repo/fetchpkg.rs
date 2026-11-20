//! The packages of a closure: downloaded in parallel from the source's mirrors in order (pacman 7.1
//! lib/libalpm/dload.c:839 curl_download_internal: several transfers at once, ParallelDownloads; :438
//! curl_retry_next_server: on any failure the next server), each as the download user into its own folder and
//! handed over (download.rs, dload.c:76 finalize_download_file), then checked: sha256 against the database's
//! %SHA256SUM% (sync.c check_validity) and the detached signature, the mirror's `.sig` or else %PGPSIG%, through
//! PK-6b's verifier with pacman's TrustedOnly (signing.c:890 _alpm_process_siglist).
//!
//! Resume: a package already complete in the cache (`/data/bitspkg/cache/<filename>`, its size and sha256 as the
//! database says) is not fetched again, so a cut after the download or the check loses nothing. Byte-range resume
//! of a half file (dload.c's `.part` with CURLOPT_RESUME_FROM) needs a `--resume` in fetch: change request to UL;
//! until then a half file is fetched again from its start.
//!
//! The cache is shared by runs that may overlap (the command line next to bitspkgd): a file is only ever replaced
//! by a rename of a checked one, and files are removed only by a run that holds the cache alone (CacheUse).

use super::download::{Fetcher, Got as Fetched};
use super::refresh::{SIG_MAX, event};
use super::syncdb::Pkg;
use crate::fsx::{self, err};
use crate::pgp::Keyring;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Transfers at once (pacman's ParallelDownloads default in pacman.conf is 5; the stick's USB link and a Mini-PC's
/// WLAN gain little beyond 4).
pub const PARALLEL: usize = 4;

pub fn cache_dir(root: &Path) -> PathBuf {
    root.join("data/bitspkg/cache")
}

const INUSE: &str = ".inuse";
/// As the store lock: a clean-up holds the cache alone for milliseconds only.
const CACHE_WAIT: Duration = Duration::from_secs(60);

/// A run's share of the cache: a shared lock (flock) on `cache/.inuse` from before the cache is looked at until
/// the run ends (a killed run never keeps it). At the end the run tries to hold the cache alone: then no other run
/// is between its download and its unpack, and the run removes its own files (plan 1.9), the names other runs
/// left in `.later-<pid>`, stale `.slot-*` folders and, when nothing is left, the folder itself (B2). A run that
/// cannot hold it alone leaves its names in `.later-<pid>` for the one that can.
pub struct CacheUse {
    dir: PathBuf,
    file: Option<fs::File>,
    names: Vec<String>,
}

impl CacheUse {
    pub fn take(dir: &Path) -> Result<CacheUse, String> {
        let start = Instant::now();
        loop {
            fs::create_dir_all(dir).map_err(|e| err(dir, e))?;
            let p = dir.join(INUSE);
            let f = fs::File::options().create(true).truncate(false).write(true).open(&p).map_err(|e| err(&p, e))?;
            match f.try_lock_shared() {
                Ok(()) => {}
                Err(fs::TryLockError::WouldBlock) if start.elapsed() < CACHE_WAIT => {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Err(fs::TryLockError::WouldBlock) => return Err(format!("{}: ein anderes bitspkg räumt den Zwischenspeicher nicht fertig auf", p.display())),
                Err(fs::TryLockError::Error(e)) => return Err(err(&p, e)),
            }
            // A clean-up may have removed the file (and the folder) between the open and the lock: then again.
            if let (Ok(a), Ok(b)) = (fs::symlink_metadata(&p), f.metadata())
                && a.ino() == b.ino()
                && a.dev() == b.dev()
            {
                return Ok(CacheUse { dir: dir.to_path_buf(), file: Some(f), names: Vec::new() });
            }
        }
    }

    /// The run is done with these package files (installed); they go when the run ends.
    pub fn done(&mut self, names: impl IntoIterator<Item = String>) {
        self.names.extend(names);
    }
}

impl Drop for CacheUse {
    fn drop(&mut self) {
        drop(self.file.take());
        let p = self.dir.join(INUSE);
        let Ok(f) = fs::File::options().write(true).open(&p) else { return };
        if f.try_lock().is_err() {
            if !self.names.is_empty() {
                let later = self.dir.join(format!(".later-{}", std::process::id()));
                let _ = fsx::write_atomic(&later, self.names.join("\n").as_bytes(), 0o644);
                event("pkg.cache.later", &[("files", &self.names.len().to_string())]);
            }
            return;
        }
        let mut names = std::mem::take(&mut self.names);
        if let Ok(rd) = fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.starts_with(".later-") {
                    if let Ok(t) = fs::read_to_string(e.path()) {
                        names.extend(t.lines().map(str::to_string));
                    }
                    let _ = fs::remove_file(e.path());
                } else if n.starts_with(".slot-") {
                    let _ = fsx::remove_tree(&e.path());
                }
            }
        }
        for n in names.iter().filter(|n| !n.is_empty() && !n.contains('/') && !n.starts_with('.')) {
            let _ = fs::remove_file(self.dir.join(n));
            let _ = fs::remove_file(self.dir.join(format!("{n}.sig")));
        }
        let _ = fs::remove_file(&p);
        let _ = fs::remove_dir(&self.dir);
        drop(f);
    }
}

#[derive(Clone, Debug)]
pub struct Job {
    pub repo: String,
    pub pkg: Pkg,
    /// The source's server URLs with `$repo`/`$arch` filled in, in order.
    pub servers: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Have {
    pub file: PathBuf,
    /// The mirror's `.sig`, when it had one.
    pub sig: Option<PathBuf>,
    /// The server it came from; empty when it was in the cache already.
    pub from: String,
    pub bytes: u64,
    /// Mirrors that failed before (reason each): the report lists them.
    pub fallbacks: Vec<String>,
}

fn sha_ok(p: &Path, pkg: &Pkg) -> bool {
    matches!(fs::symlink_metadata(p), Ok(m) if m.is_file() && m.len() == pkg.csize)
        && fsx::hash_file(p).map(|(h, n)| h == pkg.sha256 && n == pkg.csize).unwrap_or(false)
}

/// One package: from the cache when it is complete there, else from the first mirror that gives the right bytes.
fn one(fetcher: &Fetcher, cache: &Path, slot: usize, job: &Job) -> Result<Have, String> {
    let pkg = &job.pkg;
    let dst = cache.join(&pkg.filename);
    let sig_dst = cache.join(format!("{}.sig", pkg.filename));
    if sha_ok(&dst, pkg) {
        event("pkg.download.cached", &[("pkg", &pkg.name), ("file", &pkg.filename)]);
        let sig = sig_dst.is_file().then_some(sig_dst);
        return Ok(Have { file: dst, sig, from: String::new(), bytes: 0, fallbacks: Vec::new() });
    }
    // A wrong or half file in the cache is not removed here: another run may be renaming a good one into place.
    // The fresh download is renamed over it, so a name in the cache always holds a whole file.
    if job.servers.is_empty() {
        return Err(format!("{}: die Quelle {} hat keinen Server", pkg.name, job.repo));
    }
    let base = cache.join(format!(".slot-{}-{slot}", std::process::id()));
    fsx::remove_tree(&base)?;
    fsx::mkdir(&base, 0o755)?;
    // The whole-transfer deadline follows the size (R1): a large package over a slow WLAN must be able to finish.
    let fetcher = &fetcher.sized(pkg.csize);
    let r = (|| {
        let wd = fetcher.workdir(&base)?;
        let mut fallbacks = Vec::new();
        for s in &job.servers {
            let url = format!("{}/{}", s.trim_end_matches('/'), pkg.filename);
            event("pkg.download.start", &[("pkg", &pkg.name), ("url", &url)]);
            let why = match fetcher.get(&wd, &url, &pkg.filename, pkg.csize) {
                Fetched::File(p) if sha_ok(&p, pkg) => {
                    let sig = match fetcher.get(&wd, &format!("{url}.sig"), &format!("{}.sig", pkg.filename), SIG_MAX) {
                        Fetched::File(sp) => {
                            fsx::rename(&sp, &sig_dst)?;
                            Some(sig_dst.clone())
                        }
                        Fetched::Missing(_) => {
                            let _ = fs::remove_file(&sig_dst);
                            None
                        }
                        Fetched::Failed(e) => {
                            // The database's %PGPSIG% stands in; without one the check refuses the package.
                            event("pkg.download.sig_failed", &[("pkg", &pkg.name), ("url", &url), ("reason", &e)]);
                            let _ = fs::remove_file(&sig_dst);
                            None
                        }
                    };
                    fsx::rename(&p, &dst)?;
                    event("pkg.download.done", &[("pkg", &pkg.name), ("bytes", &pkg.csize.to_string()), ("from", s)]);
                    return Ok(Have { file: dst.clone(), sig, from: s.clone(), bytes: pkg.csize, fallbacks });
                }
                Fetched::File(p) => {
                    let got = fsx::hash_file(&p).map(|(h, n)| format!("sha256 {h}, {n} Bytes")).unwrap_or_else(|e| e);
                    let _ = fs::remove_file(&p);
                    format!("falscher Inhalt ({got}; die Datenbank sagt sha256 {}, {} Bytes)", pkg.sha256, pkg.csize)
                }
                Fetched::Missing(m) => format!("nicht vorhanden ({m})"),
                Fetched::Failed(m) => m,
            };
            event("pkg.download.retry", &[("pkg", &pkg.name), ("url", &url), ("reason", &why)]);
            fallbacks.push(format!("{s}: {why}"));
        }
        Err(format!("{}: von keinem Server zu bekommen ({})", pkg.filename, fallbacks.join("; ")))
    })();
    let _ = fsx::remove_tree(&base);
    r
}

/// Every job, `PARALLEL` at a time; the results in the jobs' order. The workers only download and hash: steps
/// of the cut test and the report are the caller's (one thread), so their order never depends on the network.
pub fn fetch_all(fetcher: &Fetcher, cache: &Path, jobs: &[Job]) -> Vec<Result<Have, String>> {
    let next = AtomicUsize::new(0);
    let out: Vec<Mutex<Option<Result<Have, String>>>> = jobs.iter().map(|_| Mutex::new(None)).collect();
    let workers = PARALLEL.min(jobs.len()).max(1);
    std::thread::scope(|s| {
        for slot in 0..workers {
            let (next, out) = (&next, &out);
            // B4: downloads are Background work (SCHED_IDLE is V2's Background class); the fetch children this
            // worker starts inherit the class. A refusal leaves the worker in the caller's class, with an event.
            let spawned = std::thread::Builder::new().name(format!("pkg-dl-{slot}")).spawn_scoped(s, move || {
                if let Err(e) = usys::sched_setscheduler(0, usys::SCHED_IDLE, 0) {
                    event("pkg.download.class", &[("slot", &slot.to_string()), ("errno", &e.to_string())]);
                }
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= jobs.len() {
                        break;
                    }
                    let r = one(fetcher, cache, slot, &jobs[i]);
                    if let Ok(mut g) = out[i].lock() {
                        *g = Some(r);
                    }
                }
            });
            if let Err(e) = spawned {
                event("pkg.download.thread", &[("reason", &e.to_string())]);
            }
        }
    });
    // A job no worker reached (no thread could start): fetched here, one after the other.
    out.into_iter()
        .enumerate()
        .map(|(i, m)| m.into_inner().ok().flatten().unwrap_or_else(|| one(fetcher, cache, 0, &jobs[i])))
        .collect()
}

/// The detached signature of `have`: the mirror's `.sig`, else the database's %PGPSIG% (plan 1.4 item 2, the
/// brief's order). pacman takes %PGPSIG% first when the database has one (signing.c:559/:590), so a rejected
/// mirror `.sig` is followed by %PGPSIG%: where pacman would accept a package on its database signature, bitspkg
/// does too, and the report names both verdicts. Ok: the signing key's fingerprint; Err: the German reasons.
pub fn check_sig(kr: &Keyring, pkg: &Pkg, have: &Have) -> Result<String, String> {
    let mut tries: Vec<(Result<Vec<u8>, String>, &str)> = Vec::new();
    if let Some(p) = &have.sig {
        tries.push((super::download::read_small(p, SIG_MAX), "Signatur vom Spiegel"));
    }
    if !pkg.pgpsig.is_empty() {
        tries.push((crate::pgp::packet::base64(&pkg.pgpsig).map_err(|e| format!("%PGPSIG% unlesbar: {e}")), "Signatur aus der Datenbank"));
    }
    if tries.is_empty() {
        return Err("keine Signatur (weder .sig beim Spiegel noch %PGPSIG% in der Datenbank); Paket-Signaturen sind Pflicht".into());
    }
    let mut why = Vec::new();
    for (sig, from) in tries {
        let sig = match sig {
            Ok(s) => s,
            Err(e) => {
                why.push(format!("{from} unlesbar: {e}"));
                continue;
            }
        };
        let f = fs::File::open(&have.file).map_err(|e| err(&have.file, e))?;
        let v = kr.verify_detached(f, &sig);
        if v.good {
            if !why.is_empty() {
                event("pkg.verify.fallback", &[("pkg", &pkg.name), ("from", from), ("reason", &why.join("; "))]);
            }
            event("pkg.verify.good", &[("pkg", &pkg.name), ("key", &v.key_fpr), ("from", from)]);
            return Ok(v.key_fpr);
        }
        why.push(format!("{from} abgelehnt: {}", v.reason));
    }
    Err(why.join("; "))
}

/// The sha256 against %SHA256SUM% once more, on the file as it lies in the cache now.
pub fn check_sha(pkg: &Pkg, have: &Have) -> Result<(), String> {
    match fsx::hash_file(&have.file) {
        Ok((h, n)) if h == pkg.sha256 && n == pkg.csize => Ok(()),
        Ok((h, n)) => Err(format!("sha256 {h} ({n} Bytes) statt {} ({} Bytes) laut Datenbank", pkg.sha256, pkg.csize)),
        Err(e) => Err(e),
    }
}

/// A refused or used download goes from the cache (a bad file must never be taken by the next run).
pub fn drop_cached(have: &Have) {
    let _ = fs::remove_file(&have.file);
    if let Some(s) = &have.sig {
        let _ = fs::remove_file(s);
    }
}
