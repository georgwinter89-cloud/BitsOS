//! The engine: the live index generation, the embedder, the indexer thread, and the search of DESIGN §7.1.
//! A query takes the current generation once (an `Arc`) and never waits for indexing.

use crate::config::Context;
use crate::embed::{EmbedKind, Embedder};
use crate::extract::pool::{Isolation, Pool};
use crate::extract::{EXTRACTOR_VERSION, Limits};
use crate::indexer::{self, Outcome, Progress};
use crate::kindex::KIndex;
use crate::rank::{self, Parts, Scored};
use crate::store::{Catalog, Store};
use crate::vindex::VecIndex;
use crate::workspace::WorkspaceStore;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

pub struct Generation {
    pub n: u64,
    pub cat: Catalog,
    /// chunk → (doc, start, end) byte range into the document's text.
    pub chunks: Vec<(u32, u32, u32)>,
    pub vec: Option<Arc<VecIndex>>,
    pub kidx: KIndex,
    pub by_id: HashMap<String, u32>,
}

impl Generation {
    /// Load and verify the live generation (cheap checksums, DESIGN §4.3).
    pub fn load(store: &Store, hnsw_min: usize) -> Result<Option<Generation>, String> {
        let Some(n) = store.current() else { return Ok(None) };
        let dir = store.gen_dir(n);
        let cat = Store::read_catalog(&dir)?;
        let cb = std::fs::read(dir.join("chunks.bin")).map_err(|e| format!("chunks: {e}"))?;
        if Store::bytes_sum(&cb) != cat.chunks_sum || cb.len() != cat.n_chunks * 12 {
            return Err("chunks.bin checksum mismatch".into());
        }
        let chunks: Vec<(u32, u32, u32)> = cb
            .as_chunks::<12>()
            .0
            .iter()
            .map(|c| {
                let u = |o: usize| u32::from_le_bytes([c[o], c[o + 1], c[o + 2], c[o + 3]]);
                (u(0), u(4), u(8))
            })
            .collect();
        let vec = if cat.embedder.is_some() {
            let vb = std::fs::read(dir.join("vectors.f32")).map_err(|e| format!("vectors: {e}"))?;
            if vb.len() != cat.n_chunks * cat.dim * 4 || Store::bytes_sum(&vb) != cat.vectors_sum {
                return Err("vectors.f32 checksum mismatch".into());
            }
            let data: Vec<f32> =
                vb.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
            let v = Arc::new(VecIndex::new(cat.dim, data, chunks.iter().map(|c| c.0).collect()));
            v.maybe_build_hnsw(hnsw_min);
            Some(v)
        } else {
            None
        };
        let kidx = KIndex::open(&dir.join("bm25")).map_err(|e| format!("bm25: {e}"))?;
        let by_id = cat.docs.iter().enumerate().map(|(i, d)| (d.id.clone(), i as u32)).collect();
        Ok(Some(Generation { n, cat, chunks, vec, kidx, by_id }))
    }

    pub fn chunk_text<'a>(&self, text: &'a str, chunk: u32) -> &'a str {
        let (_, s, e) = self.chunks.get(chunk as usize).copied().unwrap_or((0, 0, 0));
        text.get(s as usize..e as usize).unwrap_or("")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// The desktop, the CLI, the UI: everything Georg may see.
    Desktop,
    /// Claude over MCP: no private roots, short snippets (DESIGN §9.3).
    Mcp,
}

#[derive(Debug, Clone)]
pub struct SearchReq {
    pub query: String,
    pub limit: usize,
    pub kinds: Option<Vec<String>>,
    pub roots: Option<Vec<String>>,
    pub since: Option<i64>,
    pub caller: Caller,
    pub deadline: Duration,
    /// Documents to leave out (dismissed in the workspace).
    pub hidden: Vec<String>,
    /// Force this document into the candidates (`context_explain`).
    pub force_doc: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub doc: u32,
    pub parts: Parts,
    pub s: f32,
    pub matching: u32,
    pub cos: Option<f32>,
    pub bm25: f32,
    pub v_chunk: Option<u32>,
    pub b_chunk: Option<u32>,
    pub gate: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SearchOut {
    pub hits: Vec<Hit>,
    pub partial: Option<String>,
    pub took_ms: u64,
    pub embedder: Option<String>,
    pub weights: Option<rank::Weights>,
    pub candidates: usize,
}

pub struct EngineOpts {
    /// The binary that serves `extract-worker` (None: extraction in this process).
    pub worker_exe: Option<PathBuf>,
}

pub struct Engine {
    pub cfg: Context,
    pub store: Store,
    pub data: PathBuf,
    pub embedder: Option<Arc<dyn Embedder>>,
    pub embedder_error: Option<String>,
    current: RwLock<Option<Arc<Generation>>>,
    pub progress: Progress,
    pub pool: Pool,
    pub ws: WorkspaceStore,
    wake: (Mutex<bool>, Condvar),
    index_lock: Mutex<()>,
}

pub fn start_embedder(cfg: &Context, data: &Path) -> (Option<Arc<dyn Embedder>>, Option<String>) {
    match cfg.embedder.kind.as_str() {
        "fake" => (Some(Arc::new(crate::embed::fake::Fake::new(256))), None),
        "none" => (None, Some("embedder off (context.embedder.kind = none)".into())),
        _ => match crate::embed::llama::LlamaCpu::start(&cfg.embedder, data) {
            Ok(l) => (Some(Arc::new(l)), None),
            Err(e) => {
                crate::event!("warn", "embedder.fallback", from = "llama", to = "keyword_only", reason = e.clone());
                (None, Some(e))
            }
        },
    }
}

impl Engine {
    pub fn open(cfg: Context, data: &Path, opts: EngineOpts) -> Result<Engine, String> {
        let store = Store::open(data, true)?;
        crate::event::open_sink(data);
        let (embedder, embedder_error) = start_embedder(&cfg, data);
        let limits = Limits { max_text_bytes: (cfg.index.max_file_mb.max(1) << 20) as usize };
        let iso = match opts.worker_exe {
            Some(exe) => Isolation::Process { exe, mem_mb: cfg.index.extract_mem_mb },
            None => Isolation::InProcess,
        };
        let pool = Pool::new(iso, cfg.index.threads, Duration::from_secs(cfg.index.extract_seconds.max(1)), limits);
        let current = match Generation::load(&store, cfg.index.hnsw_min_chunks) {
            Ok(g) => g.map(Arc::new),
            Err(e) => {
                // A broken generation is derived data: rebuild instead of failing (DESIGN §4.1).
                crate::event!("warn", "recover.generation_broken", reason = e);
                None
            }
        };
        let ws = WorkspaceStore::open(&data.join("workspaces")).map_err(|e| format!("workspaces: {e}"))?;
        Ok(Engine {
            cfg,
            store,
            data: data.to_path_buf(),
            embedder,
            embedder_error,
            current: RwLock::new(current),
            progress: Progress::default(),
            pool,
            ws,
            wake: (Mutex::new(false), Condvar::new()),
            index_lock: Mutex::new(()),
        })
    }

    pub fn generation(&self) -> Option<Arc<Generation>> {
        self.current.read().ok().and_then(|g| g.clone())
    }

    /// One indexing run; swaps in the new generation when there is one.
    pub fn index_now(&self) -> Result<Outcome, String> {
        let _one = self.index_lock.lock().map_err(|_| "poisoned")?;
        let prev = self.generation();
        let ctx = indexer::Ctx {
            cfg: &self.cfg,
            store: &self.store,
            embedder: self.embedder.as_deref(),
            pool: &self.pool,
            progress: &self.progress,
        };
        let out = indexer::run_once(&ctx, prev.as_ref().map(|g| &g.cat))?;
        if out.generation.is_some() {
            let g = Generation::load(&self.store, self.cfg.index.hnsw_min_chunks)?.map(Arc::new);
            if let Ok(mut w) = self.current.write() {
                *w = g;
            }
        }
        Ok(out)
    }

    /// Wake the indexer thread now (after a `reindex` request or `forget index`).
    pub fn wake_indexer(&self) {
        if let Ok(mut w) = self.wake.0.lock() {
            *w = true;
            self.wake.1.notify_all();
        }
    }

    /// The indexer loop of `ctxd serve`: run, then sleep until the rescan interval or a wake-up.
    pub fn indexer_loop(self: &Arc<Self>) {
        loop {
            if let Err(e) = self.index_now() {
                crate::event!("warn", "index.retry_later", reason = e);
            }
            let Ok(mut w) = self.wake.0.lock() else { return };
            let wait = Duration::from_secs(self.cfg.index.rescan_seconds.max(1));
            if !*w {
                w = match self.wake.1.wait_timeout(w, wait) {
                    Ok((g, _)) => g,
                    Err(_) => return,
                };
            }
            *w = false;
        }
    }

    pub fn clear_generation(&self) {
        if let Ok(mut w) = self.current.write() {
            *w = None;
        }
    }

    pub fn doc_text(&self, g: &Generation, doc: u32) -> String {
        g.cat
            .docs
            .get(doc as usize)
            .and_then(|d| self.store.get_text(&d.hash, EXTRACTOR_VERSION))
            .map(|t| t.text)
            .unwrap_or_default()
    }

    fn allowed(&self, g: &Generation, req: &SearchReq, d: u32) -> bool {
        let Some(doc) = g.cat.docs.get(d as usize) else { return false };
        if req.caller == Caller::Mcp && doc.private {
            return false;
        }
        if req.hidden.contains(&doc.id) {
            return false;
        }
        if let Some(k) = &req.kinds
            && !k.iter().any(|k| *k == doc.kind || (k == "text" && doc.kind == "md"))
        {
            return false;
        }
        if let Some(rs) = &req.roots {
            let ok = doc.paths.iter().any(|p| {
                rs.iter().any(|r| {
                    g.cat
                        .roots
                        .get(p.root)
                        .is_some_and(|root| Path::new(&root.path) == Path::new(r) || Path::new(&root.path).ends_with(r))
                })
            });
            if !ok {
                return false;
            }
        }
        if let Some(since) = req.since
            && doc.mtime < since
        {
            return false;
        }
        true
    }

    /// The search of DESIGN §7.1. Returns every scored candidate that passed the gate, best first, cut to
    /// `limit` (with `force_doc`, that document is kept even below the gate, for `explain`).
    pub fn search(&self, req: &SearchReq) -> SearchOut {
        let t0 = Instant::now();
        let Some(g) = self.generation() else {
            return SearchOut { partial: Some("index_empty".into()), ..Default::default() };
        };
        let rc = &self.cfg.rank;
        let k = rc.candidates.max(req.limit.saturating_mul(5)).max(1);
        let allow = |d: u32| self.allowed(&g, req, d) || Some(d) == req.force_doc;
        let calib = g.cat.calib;

        // Query vector within the deadline; without it the result is partial (keyword part only).
        let mut partial = None;
        let qvec = match (&self.embedder, &g.vec) {
            (Some(emb), Some(_)) if g.cat.embedder.as_deref() == Some(emb.id()) => {
                let left = req.deadline.saturating_sub(t0.elapsed()).max(Duration::from_millis(1));
                match emb.embed(EmbedKind::Query, &[req.query.as_str()], left) {
                    Ok(mut v) => v.pop(),
                    Err(e) => {
                        partial = Some(match e {
                            crate::embed::EmbedError::Timeout => "embedder_slow".to_string(),
                            e => format!("embedder_unavailable: {e}"),
                        });
                        None
                    }
                }
            }
            (_, None) => {
                partial = Some("no_vectors_in_index".into());
                None
            }
            _ => {
                partial = Some("embedder_unavailable".into());
                None
            }
        };

        // Candidates: top K documents of each list, chunks collapsed per document before the cut.
        let mut cands: HashMap<u32, Hit> = HashMap::new();
        let new_hit = |doc| Hit {
            doc,
            parts: Parts::default(),
            s: 0.0,
            matching: 0,
            cos: None,
            bm25: 0.0,
            v_chunk: None,
            b_chunk: None,
            gate: false,
        };
        let match_cos = calib.floor + rc.v_min * (calib.ceil - calib.floor);
        let mut v_rank: HashMap<u32, usize> = HashMap::new();
        if let (Some(q), Some(vi)) = (&qvec, &g.vec) {
            for (i, h) in vi.top_docs(q, k, match_cos, &allow).into_iter().enumerate() {
                let e = cands.entry(h.doc).or_insert_with(|| new_hit(h.doc));
                e.cos = Some(h.cos);
                e.v_chunk = Some(h.chunk);
                e.matching = e.matching.max(h.matching);
                v_rank.insert(h.doc, i);
            }
        }
        let mut b_rank: HashMap<u32, usize> = HashMap::new();
        let mut want = k * 4;
        loop {
            let hits = g.kidx.search(&req.query, want).unwrap_or_default();
            let mut per: HashMap<u32, (f32, u32, u32)> = HashMap::new();
            for (c, s) in &hits {
                let Some(&(d, _, _)) = g.chunks.get(*c as usize) else { continue };
                if !allow(d) {
                    continue;
                }
                let e = per.entry(d).or_insert((f32::MIN, *c, 0));
                if *s > e.0 {
                    e.0 = *s;
                    e.1 = *c;
                }
                e.2 += 1;
            }
            if per.len() >= k || hits.len() < want {
                let mut list: Vec<(u32, (f32, u32, u32))> = per.into_iter().collect();
                list.sort_by(|a, b| b.1.0.total_cmp(&a.1.0).then(a.0.cmp(&b.0)));
                list.truncate(k);
                for (i, (d, (s, c, n))) in list.into_iter().enumerate() {
                    let e = cands.entry(d).or_insert_with(|| new_hit(d));
                    e.bm25 = s;
                    e.b_chunk = Some(c);
                    e.matching = e.matching.max(n);
                    b_rank.insert(d, i);
                }
                break;
            }
            want *= 2;
        }
        if let Some(fd) = req.force_doc {
            cands.entry(fd).or_insert_with(|| new_hit(fd));
        }
        // Keyword-only candidates still get their V (one document's chunks, cheap).
        if let (Some(q), Some(vi)) = (&qvec, &g.vec) {
            for h in cands.values_mut().filter(|h| h.cos.is_none()) {
                if let Some(d) = g.cat.docs.get(h.doc as usize)
                    && let Some((cos, c)) = vi.doc_best(q, d.chunk_start as usize, d.chunk_count as usize)
                {
                    h.cos = Some(cos);
                    h.v_chunk = Some(c);
                }
            }
        }

        let max_b = cands.values().map(|h| h.bm25).fold(0.0f32, f32::max);
        let w = rank::weights(rc, qvec.is_some(), false);
        let now = crate::util::now_secs();
        let mut scored = Vec::with_capacity(cands.len());
        for h in cands.values_mut() {
            let d = &g.cat.docs[h.doc as usize];
            let age_days = (now - d.mtime) as f32 / 86_400.0;
            h.parts = Parts {
                v: if qvec.is_some() { h.cos.map(|c| rank::calibrated(c, calib.floor, calib.ceil)) } else { None },
                b: if max_b > 0.0 { h.bm25 / max_b } else { 0.0 },
                c: None,
                r: rank::recency(age_days, rc.t_half_days),
                x: 0.0,
            };
            h.gate = rank::passes_gate(&h.parts, rc);
            h.s = if rc.fusion == "rrf" {
                rank::rrf(v_rank.get(&h.doc).copied(), b_rank.get(&h.doc).copied()) * 30.0 + w.r * h.parts.r * 0.01
            } else {
                rank::score(&h.parts, &w)
            };
            if h.gate || Some(h.doc) == req.force_doc {
                scored.push(Scored { doc: h.doc, s: h.s, matching: h.matching, r: h.parts.r });
            }
        }
        let n_cands = cands.len();
        rank::sort(&mut scored);
        let mut hits: Vec<Hit> = Vec::with_capacity(req.limit.min(scored.len()));
        for (i, s) in scored.into_iter().enumerate() {
            if (i < req.limit || Some(s.doc) == req.force_doc)
                && let Some(h) = cands.remove(&s.doc)
            {
                hits.push(h);
            }
        }
        let took = t0.elapsed().as_millis() as u64;
        let m = &crate::event::METRICS;
        m.searches.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        m.search_ms_total.fetch_add(took, std::sync::atomic::Ordering::Relaxed);
        if partial.is_some() {
            m.search_partial.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        crate::event!("info", "search", took_ms = took, results = hits.len(), partial = partial.is_some());
        SearchOut {
            hits,
            partial,
            took_ms: took,
            embedder: g.cat.embedder.clone(),
            weights: Some(w),
            candidates: n_cands,
        }
    }
}
