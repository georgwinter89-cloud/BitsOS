//! Vector search over chunk vectors (DESIGN §6.3): an exact scan while the chunk count is below
//! `hnsw_min_chunks` (a speed switch, not a limit), HNSW above it (instant-distance, built lazily in a
//! background thread; until it is ready, and whenever a query needs more candidates than the graph's
//! `ef_search` returns, the exact scan answers).

use instant_distance::{Builder, HnswMap, Point, Search};
use std::sync::{Arc, OnceLock};

#[derive(Clone)]
pub struct VPoint(Arc<[f32]>);

impl Point for VPoint {
    fn distance(&self, other: &Self) -> f32 {
        1.0 - crate::embed::dot(&self.0, &other.0)
    }
}

pub struct VecIndex {
    pub dim: usize,
    data: Vec<f32>,
    /// chunk → document index.
    chunk_doc: Vec<u32>,
    hnsw: Arc<OnceLock<HnswMap<VPoint, u32>>>,
    pub ef_search: usize,
}

/// Per document: best cosine, its chunk, number of chunks above `match_cos`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DocHit {
    pub doc: u32,
    pub cos: f32,
    pub chunk: u32,
    pub matching: u32,
}

impl VecIndex {
    pub fn new(dim: usize, data: Vec<f32>, chunk_doc: Vec<u32>) -> VecIndex {
        VecIndex { dim, data, chunk_doc, hnsw: Arc::new(OnceLock::new()), ef_search: 256 }
    }

    pub fn len(&self) -> usize {
        self.chunk_doc.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chunk_doc.is_empty()
    }

    pub fn vector(&self, chunk: usize) -> &[f32] {
        &self.data[chunk * self.dim..(chunk + 1) * self.dim]
    }

    /// Start building the graph in the background when the index is large enough.
    pub fn maybe_build_hnsw(self: &Arc<Self>, min_chunks: usize) {
        if self.len() < min_chunks.max(1) || self.hnsw.get().is_some() {
            return;
        }
        let me = Arc::clone(self);
        let _ = std::thread::Builder::new().name("ctxd-hnsw".into()).spawn(move || {
            let t = std::time::Instant::now();
            let pts: Vec<VPoint> = (0..me.len()).map(|c| VPoint(me.vector(c).into())).collect();
            let vals: Vec<u32> = (0..me.len() as u32).collect();
            // M is fixed by instant-distance (32); efConstruction 200, efSearch from the index.
            let map = Builder::default().ef_construction(200).ef_search(me.ef_search).seed(7).build(pts, vals);
            let _ = me.hnsw.set(map);
            crate::event!("info", "index.hnsw_ready", chunks = me.len(), ms = t.elapsed().as_millis() as u64);
        });
    }

    pub fn hnsw_ready(&self) -> bool {
        self.hnsw.get().is_some()
    }

    fn collapse(
        &self,
        hits: impl Iterator<Item = (u32, f32)>,
        match_cos: f32,
        allow: &dyn Fn(u32) -> bool,
    ) -> Vec<DocHit> {
        let mut best: std::collections::HashMap<u32, DocHit> = std::collections::HashMap::new();
        for (c, cos) in hits {
            let d = self.chunk_doc[c as usize];
            if !allow(d) {
                continue;
            }
            let e = best.entry(d).or_insert(DocHit { doc: d, cos: f32::MIN, chunk: c, matching: 0 });
            if cos > e.cos {
                e.cos = cos;
                e.chunk = c;
            }
            if cos >= match_cos {
                e.matching += 1;
            }
        }
        let mut v: Vec<DocHit> = best.into_values().collect();
        v.sort_by(|a, b| b.cos.total_cmp(&a.cos).then(a.doc.cmp(&b.doc)));
        v
    }

    /// Top `k` documents for `q` (chunks collapsed per document before the cut, DESIGN §7.1).
    pub fn top_docs(&self, q: &[f32], k: usize, match_cos: f32, allow: &dyn Fn(u32) -> bool) -> Vec<DocHit> {
        if q.len() != self.dim || k == 0 {
            return Vec::new();
        }
        if let Some(map) = self.hnsw.get() {
            let mut s = Search::default();
            let hits: Vec<(u32, f32)> =
                map.search(&VPoint(q.into()), &mut s).map(|i| (*i.value, 1.0 - i.distance)).collect();
            let mut v = self.collapse(hits.into_iter(), match_cos, allow);
            if v.len() >= k {
                v.truncate(k);
                return v;
            }
        }
        let hits = (0..self.len()).map(|c| (c as u32, crate::embed::dot(q, self.vector(c))));
        let mut v = self.collapse(hits, match_cos, allow);
        v.truncate(k);
        v
    }

    /// Best cosine of one document's chunks (for `explain` and for keyword-only candidates).
    pub fn doc_best(&self, q: &[f32], start: usize, count: usize) -> Option<(f32, u32)> {
        (start..start + count)
            .filter(|&c| c < self.len())
            .map(|c| (crate::embed::dot(q, self.vector(c)), c as u32))
            .max_by(|a, b| a.0.total_cmp(&b.0))
    }

    /// Calibration (DESIGN §6.3): cosine of random chunk pairs from different documents. e5-style models
    /// give 0.7-0.9 even for unrelated text, so the formula works on (cos - floor) / (ceil - floor).
    pub fn calibrate(&self) -> crate::store::Calib {
        let n = self.len();
        if n < 2 {
            return crate::store::Calib { floor: 0.0, ceil: 1.0 };
        }
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut cos = Vec::new();
        for _ in 0..(n * 4).clamp(64, 4000) {
            let (a, b) = ((next() % n as u64) as usize, (next() % n as u64) as usize);
            if self.chunk_doc[a] != self.chunk_doc[b] {
                cos.push(crate::embed::dot(self.vector(a), self.vector(b)));
            }
        }
        if cos.is_empty() {
            return crate::store::Calib { floor: 0.0, ceil: 1.0 };
        }
        cos.sort_by(f32::total_cmp);
        let pct = |p: f32| cos[((cos.len() - 1) as f32 * p) as usize];
        // Floor: a typical unrelated pair. Ceil: well above the top of the unrelated pairs.
        let floor = pct(0.5);
        let ceil = (pct(0.99) + 2.0 * (pct(0.99) - floor)).min(1.0).max(floor + 0.05);
        crate::store::Calib { floor, ceil }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(v: &[f32]) -> Vec<f32> {
        let mut v = v.to_vec();
        crate::embed::normalize(&mut v);
        v
    }

    #[test]
    fn exact_and_hnsw_agree() {
        let dim = 8;
        let mut data = Vec::new();
        let mut chunk_doc = Vec::new();
        let mut x: u32 = 1;
        for c in 0..600u32 {
            let v: Vec<f32> = (0..dim)
                .map(|_| {
                    x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                    (x >> 8) as f32 / (1u32 << 24) as f32 - 0.5
                })
                .collect();
            data.extend(unit(&v));
            chunk_doc.push(c / 3);
        }
        let idx = Arc::new(VecIndex::new(dim, data, chunk_doc));
        let q = idx.vector(30).to_vec();
        let exact = idx.top_docs(&q, 5, 0.9, &|_| true);
        assert_eq!(exact[0].doc, 10);
        assert!((exact[0].cos - 1.0).abs() < 1e-5);
        idx.maybe_build_hnsw(100);
        for _ in 0..200 {
            if idx.hnsw_ready() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(idx.hnsw_ready());
        let approx = idx.top_docs(&q, 5, 0.9, &|_| true);
        assert_eq!(approx[0].doc, 10);
        // The filter is applied before the cut.
        let filtered = idx.top_docs(&q, 5, 0.9, &|d| d != 10);
        assert!(filtered.iter().all(|h| h.doc != 10) && filtered.len() == 5);
        let c = idx.calibrate();
        assert!(c.ceil > c.floor);
    }
}
