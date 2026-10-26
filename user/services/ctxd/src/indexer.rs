//! The indexing pipeline (DESIGN §5): scan → hash → extract (child processes) → chunk → embed → keyword
//! index → commit a new generation. Idempotent and incremental: unchanged content (same hash) is never
//! extracted or embedded again (text and vectors are cached per content hash), so a crash or a restart
//! only repeats the batch in flight. An unchanged tree makes no new generation at all.

use crate::chunk::chunk;
use crate::config::Context;
use crate::embed::{EmbedKind, Embedder, passage_text};
use crate::extract::{EXTRACTOR_VERSION, display_title, pool::Pool};
use crate::kindex;
use crate::roots::{self, FileInfo};
use crate::store::{CachedText, Catalog, DocRec, PathRec, RootRec, Store};
use crate::vindex::VecIndex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How close to a scan a change counts as "racy" (timestamp ticks on ext4/btrfs are far below this).
const RACY_NS: i64 = 2_000_000_000;

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[derive(Default)]
pub struct Progress {
    pub running: AtomicBool,
    pub phase: Mutex<String>,
    pub total: AtomicU64,
    pub done: AtomicU64,
    pub last_error: Mutex<Option<String>>,
    pub last_finished: AtomicU64,
    pub runs: AtomicU64,
}

impl Progress {
    fn set(&self, phase: &str, total: u64) {
        if let Ok(mut p) = self.phase.lock() {
            *p = phase.to_string();
        }
        self.total.store(total, Ordering::Relaxed);
        self.done.store(0, Ordering::Relaxed);
        crate::event::METRICS.queue_len.store(total, Ordering::Relaxed);
    }
    fn tick(&self) {
        let d = self.done.fetch_add(1, Ordering::Relaxed) + 1;
        let t = self.total.load(Ordering::Relaxed);
        crate::event::METRICS.queue_len.store(t.saturating_sub(d), Ordering::Relaxed);
    }
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "running": self.running.load(Ordering::Relaxed),
            "phase": self.phase.lock().map(|p| p.clone()).unwrap_or_default(),
            "total": self.total.load(Ordering::Relaxed),
            "done": self.done.load(Ordering::Relaxed),
            "last_error": self.last_error.lock().ok().and_then(|e| e.clone()),
            "last_finished": self.last_finished.load(Ordering::Relaxed),
            "runs": self.runs.load(Ordering::Relaxed),
        })
    }
}

pub struct Ctx<'a> {
    pub cfg: &'a Context,
    pub store: &'a Store,
    pub embedder: Option<&'a dyn Embedder>,
    pub pool: &'a Pool,
    pub progress: &'a Progress,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Outcome {
    /// None: nothing changed, no new generation.
    pub generation: Option<u64>,
    pub docs: usize,
    pub chunks: usize,
    pub extracted: usize,
    pub failed: usize,
    pub embedded_chunks: usize,
    pub seconds: f64,
}

fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().to_hex().to_string())
}

pub fn chunk_sig(cfg: &Context) -> String {
    let s = format!("{}-{}-{EXTRACTOR_VERSION}", cfg.index.chunk_bytes, cfg.index.chunk_overlap);
    Store::bytes_sum(s.as_bytes())[..12].to_string()
}

/// File name and path segments below the root: their own keyword field (folder names carry context).
fn name_field(path: &Path, root: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.iter().map(|s| s.to_string_lossy()).collect::<Vec<_>>().join(" ").replace(['_', '-', '.'], " ")
}

/// Run `f` over `items` on `threads` threads (work queue, no fixed batch sizes).
fn parallel<T: Send, R: Send>(items: Vec<T>, threads: usize, f: impl Fn(T) -> R + Sync) -> Vec<R> {
    let q = Mutex::new(items.into_iter().enumerate().collect::<Vec<_>>());
    let out = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..threads.max(1) {
            s.spawn(|| {
                loop {
                    let next = q.lock().ok().and_then(|mut q| q.pop());
                    let Some((i, item)) = next else { break };
                    let r = f(item);
                    if let Ok(mut o) = out.lock() {
                        o.push((i, r));
                    }
                }
            });
        }
    });
    let mut v = out.into_inner().unwrap_or_default();
    v.sort_by_key(|x| x.0);
    v.into_iter().map(|x| x.1).collect()
}

/// Calibration for queries (DESIGN §6.3), measured on this corpus: document titles stand in for queries.
/// floor = median cosine of a title query to chunks of other documents; ceil = median cosine of a title
/// query to the best chunk of its own document. Query-to-passage cosines of e5 sit higher and closer
/// together than passage pairs, so passage pairs alone gave a floor that gated most true results.
fn calibrate_with_titles(
    emb: &dyn Embedder,
    docs: &[DocRec],
    vi: &VecIndex,
    timeout: Duration,
) -> Option<crate::store::Calib> {
    let picks: Vec<usize> = {
        let with_title: Vec<usize> = (0..docs.len()).filter(|&i| !docs[i].title.trim().is_empty()).collect();
        let step = (with_title.len() / 200).max(1);
        with_title.into_iter().step_by(step).collect()
    };
    if picks.len() < 8 {
        return None;
    }
    let mut own = Vec::new();
    let mut other = Vec::new();
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    for batch in picks.chunks(emb.batch().max(1)) {
        let titles: Vec<&str> = batch.iter().map(|&i| docs[i].title.as_str()).collect();
        let qs = emb.embed(EmbedKind::Query, &titles, timeout).ok()?;
        for (&i, q) in batch.iter().zip(&qs) {
            let d = &docs[i];
            own.push(vi.doc_best(q, d.chunk_start as usize, d.chunk_count as usize)?.0);
            for _ in 0..16 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let c = (x % vi.len() as u64) as usize;
                if !(d.chunk_start as usize..(d.chunk_start + d.chunk_count) as usize).contains(&c) {
                    other.push(crate::embed::dot(q, vi.vector(c)));
                }
            }
        }
    }
    let med = |v: &mut Vec<f32>| {
        v.sort_by(f32::total_cmp);
        v[v.len() / 2]
    };
    let floor = med(&mut other);
    let ceil = med(&mut own).max(floor + 0.02);
    crate::event!("info", "index.calibrated", floor = floor, ceil = ceil, samples = picks.len());
    Some(crate::store::Calib { floor, ceil })
}

pub fn run_once(ctx: &Ctx, prev: Option<&Catalog>) -> Result<Outcome, String> {
    let t0 = Instant::now();
    let p = ctx.progress;
    p.running.store(true, Ordering::Relaxed);
    let r = run_inner(ctx, prev, t0);
    p.running.store(false, Ordering::Relaxed);
    p.runs.fetch_add(1, Ordering::Relaxed);
    p.set("idle", 0);
    match &r {
        Ok(o) => {
            if let Ok(mut e) = p.last_error.lock() {
                *e = None;
            }
            p.last_finished.store(crate::util::now_secs() as u64, Ordering::Relaxed);
            if o.generation.is_some() {
                crate::event!(
                    "info",
                    "index.finished",
                    docs = o.docs,
                    chunks = o.chunks,
                    extracted = o.extracted,
                    failed = o.failed,
                    embedded = o.embedded_chunks,
                    seconds = o.seconds
                );
            }
        }
        Err(e) => {
            crate::event!("error", "index.failed", reason = e.clone());
            if let Ok(mut le) = p.last_error.lock() {
                *le = Some(e.clone());
            }
        }
    }
    r
}

fn run_inner(ctx: &Ctx, prev: Option<&Catalog>, t0: Instant) -> Result<Outcome, String> {
    let cfg = ctx.cfg;
    let p = ctx.progress;
    p.set("scan", 0);
    let (roots, denied) = roots::resolve(&cfg.roots, &cfg.forbidden_roots);
    let scan_ns = now_ns();
    let scan = roots::scan(&roots, &cfg.excludes, cfg.index.max_file_mb << 20);
    let root_recs: Vec<RootRec> =
        roots.iter().map(|r| RootRec { path: r.path.display().to_string(), private: r.private }).collect();
    let emb_id = ctx.embedder.map(|e| e.id().to_string());
    let dim = ctx.embedder.map(|e| e.dim()).unwrap_or(0);
    let sig = chunk_sig(cfg);

    // 1. Content hashes, reusing the previous catalog when (dev, inode, size, mtime, ctime, path) are unchanged
    //    in nanoseconds and the earlier hash was not taken in the same moment as a write ("racy").
    let mut prev_path: HashMap<(u64, u64), (&PathRec, &DocRec)> = HashMap::new();
    if let Some(c) = prev {
        for d in &c.docs {
            for pr in &d.paths {
                prev_path.insert((pr.dev, pr.ino), (pr, d));
            }
        }
    }
    p.set("hash", scan.files.len() as u64);
    let hashed: Vec<(FileInfo, Option<String>)> = parallel(scan.files.clone(), cfg.index.threads, |f| {
        let known = prev_path.get(&(f.dev, f.ino)).and_then(|(pr, d)| {
            (!pr.racy
                && pr.size == f.size
                && pr.mtime_ns == f.mtime_ns
                && pr.ctime_ns == f.ctime_ns
                && Path::new(&pr.path) == f.path)
                .then(|| d.hash.clone())
        });
        let h = known.or_else(|| hash_file(&f.path).ok());
        p.tick();
        (f, h)
    });
    let mut groups: BTreeMap<String, Vec<FileInfo>> = BTreeMap::new();
    for (f, h) in hashed {
        if let Some(h) = h {
            groups.entry(h).or_default().push(f);
        }
    }

    // Nothing changed: same files, same content, same roots, same embedder and chunking.
    if let Some(c) = prev {
        type Key = (String, String, u64, i64, i64);
        let now: HashSet<Key> = groups
            .iter()
            .flat_map(|(h, fs)| {
                fs.iter().map(move |f| (f.path.display().to_string(), h.clone(), f.size, f.mtime_ns, f.ctime_ns))
            })
            .collect();
        let before: HashSet<Key> = c
            .docs
            .iter()
            .flat_map(|d| {
                d.paths.iter().map(move |pr| (pr.path.clone(), d.hash.clone(), pr.size, pr.mtime_ns, pr.ctime_ns))
            })
            .collect();
        if now == before && c.embedder == emb_id && c.chunk_sig == sig && c.roots == root_recs {
            return Ok(Outcome {
                docs: c.docs.len(),
                chunks: c.n_chunks,
                seconds: t0.elapsed().as_secs_f64(),
                ..Default::default()
            });
        }
    }
    crate::event!(
        "info",
        "index.started",
        files = scan.files.len(),
        contents = groups.len(),
        excluded = scan.excluded,
        too_large = scan.too_large,
        symlinks = scan.symlinks
    );

    // 2. Extraction of contents without cached text, in the worker processes.
    let todo: Vec<(String, std::path::PathBuf)> = groups
        .iter()
        .filter(|(h, _)| ctx.store.get_text(h, EXTRACTOR_VERSION).is_none())
        .map(|(h, fs)| (h.clone(), fs[0].path.clone()))
        .collect();
    let extracted = todo.len();
    p.set("extract", todo.len() as u64);
    let failed = AtomicU64::new(0);
    parallel(todo, cfg.index.threads, |(h, path)| {
        let ct = match ctx.pool.extract(&path) {
            Ok(e) => {
                let status = if e.text.trim().is_empty() { "empty".to_string() } else { "ok".to_string() };
                CachedText {
                    extractor: EXTRACTOR_VERSION.into(),
                    kind: e.kind,
                    title: e.title,
                    status,
                    text: e.text,
                    meta: e.meta,
                    warnings: e.warnings,
                    sum: String::new(),
                }
            }
            Err(reason) => {
                failed.fetch_add(1, Ordering::Relaxed);
                crate::event::METRICS.extract_failed_total.fetch_add(1, Ordering::Relaxed);
                crate::event!("warn", "doc.extract_failed", hash = h[..16].to_string(), reason = reason.clone());
                let kind = crate::extract::sniff(&path, &[]).name().to_string();
                CachedText {
                    extractor: EXTRACTOR_VERSION.into(),
                    kind,
                    title: None,
                    status: format!("failed:{reason}"),
                    text: String::new(),
                    meta: vec![],
                    warnings: vec![],
                    sum: String::new(),
                }
            }
        };
        if let Err(e) = ctx.store.put_text(&h, ct) {
            crate::event!("error", "store.write_failed", what = "text", error = e.to_string());
        }
        p.tick();
    });

    // 3. Documents with stable ids, chunk ranges.
    let mut prev_hash: HashMap<&str, &str> = HashMap::new();
    if let Some(c) = prev {
        for d in &c.docs {
            prev_hash.entry(d.hash.as_str()).or_insert(d.id.as_str());
        }
    }
    let mut used = HashSet::new();
    let mut docs: Vec<DocRec> = Vec::with_capacity(groups.len());
    let mut ranges: Vec<Vec<(u32, u32)>> = Vec::with_capacity(groups.len());
    for (h, fs) in &groups {
        let Some(ct) = ctx.store.get_text(h, EXTRACTOR_VERSION) else { continue };
        let by_inode = fs.iter().find_map(|f| prev_path.get(&(f.dev, f.ino)).map(|(_, d)| d.id.as_str()));
        let id = by_inode
            .or_else(|| prev_hash.get(h.as_str()).copied())
            .filter(|id| !used.contains(*id))
            .map(String::from)
            .unwrap_or_else(|| crate::util::new_id("d"));
        used.insert(id.clone());
        let r: Vec<(u32, u32)> = chunk(&ct.text, cfg.index.chunk_bytes, cfg.index.chunk_overlap)
            .into_iter()
            .map(|(s, e)| (s as u32, e as u32))
            .collect();
        // A document without text still gets one (empty) chunk: it stays findable by title and name.
        let r = if r.is_empty() { vec![(0, 0)] } else { r };
        let paths: Vec<PathRec> = fs
            .iter()
            .map(|f| PathRec {
                path: f.path.display().to_string(),
                root: f.root,
                dev: f.dev,
                ino: f.ino,
                size: f.size,
                mtime: f.mtime,
                mtime_ns: f.mtime_ns,
                ctime_ns: f.ctime_ns,
                racy: f.mtime_ns.max(f.ctime_ns) >= scan_ns.saturating_sub(RACY_NS),
            })
            .collect();
        docs.push(DocRec {
            id,
            private: fs.iter().any(|f| roots[f.root].private),
            mtime: fs.iter().map(|f| f.mtime).max().unwrap_or(0),
            size: fs[0].size,
            title: display_title(ct.title.as_deref(), &fs[0].path),
            hash: h.clone(),
            kind: ct.kind.clone(),
            status: ct.status.clone(),
            paths,
            chunk_start: 0,
            chunk_count: r.len() as u32,
            warnings: ct.warnings.clone(),
            meta: ct.meta.clone(),
        });
        ranges.push(r);
    }
    let mut start = 0u32;
    for d in docs.iter_mut() {
        d.chunk_start = start;
        start += d.chunk_count;
    }
    let n_chunks = start as usize;

    // 4. Vectors: cached per (embedder, content, chunking, title); the rest in batches.
    let mut vectors: Vec<f32> = Vec::new();
    let mut embedded_chunks = 0;
    if let Some(emb) = ctx.embedder {
        let eid = emb.id().to_string();
        let mut per_doc: Vec<Option<Vec<f32>>> = vec![None; docs.len()];
        let mut need: Vec<usize> = Vec::new();
        for (i, d) in docs.iter().enumerate() {
            let key = format!("{sig}{}", &Store::bytes_sum(d.title.as_bytes())[..8]);
            match ctx.store.get_vecs(&eid, &d.hash, &key, dim) {
                Some((r, v)) if r == ranges[i] => per_doc[i] = Some(v),
                _ => need.push(i),
            }
        }
        let need_chunks: usize = need.iter().map(|&i| ranges[i].len()).sum();
        p.set("embed", need_chunks as u64);
        let batch = emb.batch().max(1);
        let timeout = Duration::from_millis(cfg.embedder.batch_timeout_ms.max(1000));
        // Work list of (doc, chunk index); a doc's vectors are cached as soon as all its chunks are done.
        let mut pending: HashMap<usize, (usize, Vec<f32>)> = HashMap::new();
        let mut queue: Vec<(usize, usize)> =
            need.iter().flat_map(|&i| (0..ranges[i].len()).map(move |c| (i, c))).collect();
        queue.reverse();
        let mut cur: Option<(usize, CachedText)> = None;
        while !queue.is_empty() {
            let take: Vec<(usize, usize)> = (0..batch).filter_map(|_| queue.pop()).collect();
            let mut texts = Vec::with_capacity(take.len());
            for &(i, c) in &take {
                if cur.as_ref().is_none_or(|(ci, _)| *ci != i) {
                    cur = Some((i, ctx.store.get_text(&docs[i].hash, EXTRACTOR_VERSION).ok_or("text cache vanished")?));
                }
                let ct = &cur.as_ref().ok_or("text cache vanished")?.1;
                let (s, e) = ranges[i][c];
                texts.push(passage_text(&docs[i].title, ct.text.get(s as usize..e as usize).unwrap_or("")));
            }
            let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
            let vecs = emb.embed(EmbedKind::Passage, &refs, timeout).map_err(|e| {
                crate::event!("error", "embedder.failed", reason = e.to_string());
                format!("embedder: {e}")
            })?;
            for (&(i, _), v) in take.iter().zip(vecs) {
                let e = pending.entry(i).or_insert_with(|| (0, Vec::with_capacity(ranges[i].len() * dim)));
                e.0 += 1;
                e.1.extend_from_slice(&v);
                p.tick();
                if e.0 == ranges[i].len() {
                    let (_, v) = pending.remove(&i).unwrap_or_default();
                    let key = format!("{sig}{}", &Store::bytes_sum(docs[i].title.as_bytes())[..8]);
                    if let Err(e) = ctx.store.put_vecs(&eid, &docs[i].hash, &key, dim, &ranges[i], &v) {
                        crate::event!("error", "store.write_failed", what = "vectors", error = e.to_string());
                    }
                    per_doc[i] = Some(v);
                }
            }
            embedded_chunks += take.len();
        }
        vectors.reserve(n_chunks * dim);
        for v in per_doc {
            vectors.extend(v.ok_or("vectors missing after embedding")?);
        }
    }

    // 5. A new generation: chunks, vectors, keyword index, catalog; then commit.
    p.set("commit", 1);
    let (gn, gdir) = ctx.store.begin_generation().map_err(|e| format!("generation: {e}"))?;
    let mut chunk_bin = Vec::with_capacity(n_chunks * 12);
    let mut chunk_doc = Vec::with_capacity(n_chunks);
    for (i, r) in ranges.iter().enumerate() {
        for (s, e) in r {
            chunk_bin.extend_from_slice(&(i as u32).to_le_bytes());
            chunk_bin.extend_from_slice(&s.to_le_bytes());
            chunk_bin.extend_from_slice(&e.to_le_bytes());
            chunk_doc.push(i as u32);
        }
    }
    let werr = |e: std::io::Error| format!("write generation: {e}");
    std::fs::write(gdir.join("chunks.bin"), &chunk_bin).map_err(werr)?;
    let mut calib = crate::store::Calib { floor: 0.0, ceil: 1.0 };
    let mut vectors_sum = String::new();
    if ctx.embedder.is_some() {
        let mut f = std::io::BufWriter::new(std::fs::File::create(gdir.join("vectors.f32")).map_err(werr)?);
        let mut h = blake3::Hasher::new();
        for v in &vectors {
            let b = v.to_le_bytes();
            h.update(&b);
            f.write_all(&b).map_err(werr)?;
        }
        f.flush().map_err(werr)?;
        vectors_sum = h.finalize().to_hex()[..32].to_string();
        let vi = VecIndex::new(dim, vectors, chunk_doc);
        calib = match ctx.embedder {
            Some(emb) => {
                calibrate_with_titles(emb, &docs, &vi, Duration::from_millis(cfg.embedder.batch_timeout_ms.max(1000)))
                    .unwrap_or_else(|| vi.calibrate())
            }
            None => vi.calibrate(),
        };
    }
    {
        let texts: Vec<Option<CachedText>> =
            docs.iter().map(|d| ctx.store.get_text(&d.hash, EXTRACTOR_VERSION)).collect();
        let names: Vec<String> = docs
            .iter()
            .map(|d| {
                let pr = &d.paths[0];
                name_field(Path::new(&pr.path), Path::new(&root_recs[pr.root].path))
            })
            .collect();
        let entries = docs.iter().enumerate().flat_map(|(i, d)| {
            let text = texts[i].as_ref().map(|t| t.text.as_str()).unwrap_or("");
            let title = d.title.as_str();
            let name = names[i].as_str();
            ranges[i].iter().enumerate().map(move |(c, &(s, e))| kindex::Entry {
                chunk: d.chunk_start + c as u32,
                title,
                name,
                body: text.get(s as usize..e as usize).unwrap_or(""),
            })
        });
        kindex::build(&gdir.join("bm25"), entries, cfg.index.threads).map_err(|e| format!("keyword index: {e}"))?;
    }
    let cat = Catalog {
        version: 1,
        generation: gn,
        built: crate::util::now_secs(),
        embedder: emb_id,
        dim,
        chunk_sig: sig,
        roots: root_recs,
        denied,
        n_chunks,
        chunks_sum: Store::bytes_sum(&chunk_bin),
        vectors_sum,
        calib,
        scan: serde_json::json!({
            "files": scan.files.len(), "excluded": scan.excluded, "too_large": scan.too_large,
            "symlinks": scan.symlinks, "other_fs": scan.other_fs, "bad_names": scan.bad_names, "errors": scan.errors,
        }),
        docs,
    };
    Store::write_catalog(&gdir, &cat).map_err(werr)?;
    ctx.store.commit(gn).map_err(|e| format!("commit: {e}"))?;
    ctx.store.gc();
    crate::event::METRICS.docs.store(cat.docs.len() as u64, Ordering::Relaxed);
    crate::event::METRICS.chunks.store(n_chunks as u64, Ordering::Relaxed);
    Ok(Outcome {
        generation: Some(gn),
        docs: cat.docs.len(),
        chunks: n_chunks,
        extracted,
        failed: failed.load(Ordering::Relaxed) as usize,
        embedded_chunks,
        seconds: t0.elapsed().as_secs_f64(),
    })
}
