//! Quality numbers on the labelled test set (`host/cx-eval`, DESIGN §11.1): capped recall@10 per topic for
//! `query` and `query_alt`, precision@10, nDCG@10, MRR, near-distractor leakage, recall on the hard documents.

use crate::engine::{Caller, Engine, SearchReq};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Deserialize)]
pub struct Topic {
    pub id: String,
    pub name: String,
    pub query: String,
    pub query_alt: String,
    pub relevant: Vec<String>,
    #[serde(default)]
    pub near_distractors: Vec<String>,
    #[serde(default)]
    pub hard: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Topics {
    topics: Vec<Topic>,
}

pub fn load_topics(path: &Path) -> Result<Vec<Topic>, String> {
    let b = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(serde_json::from_slice::<Topics>(&b).map_err(|e| format!("topics: {e}"))?.topics)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Metrics {
    pub recall: f64,
    pub precision: f64,
    pub ndcg: f64,
    pub mrr: f64,
    pub leakage: f64,
    pub hard_recall: Option<f64>,
}

pub fn metrics(top: &[String], t: &Topic) -> Metrics {
    let rel: HashSet<&str> = t.relevant.iter().map(String::as_str).collect();
    let top10 = &top[..top.len().min(10)];
    let hits = top10.iter().filter(|d| rel.contains(d.as_str())).count();
    let cap = rel.len().clamp(1, 10);
    let dcg: f64 = top10
        .iter()
        .enumerate()
        .filter(|(_, d)| rel.contains(d.as_str()))
        .map(|(i, _)| 1.0 / (i as f64 + 2.0).log2())
        .sum();
    let idcg: f64 = (0..cap).map(|i| 1.0 / (i as f64 + 2.0).log2()).sum();
    let mrr = top.iter().position(|d| rel.contains(d.as_str())).map(|p| 1.0 / (p as f64 + 1.0)).unwrap_or(0.0);
    let leak = top10.iter().filter(|d| t.near_distractors.contains(d)).count();
    let hard_recall = (!t.hard.is_empty())
        .then(|| top10.iter().filter(|d| t.hard.contains(d)).count() as f64 / t.hard.len().min(10) as f64);
    Metrics {
        recall: hits as f64 / cap as f64,
        precision: hits as f64 / 10.0,
        ndcg: dcg / idcg,
        mrr,
        leakage: leak as f64,
        hard_recall,
    }
}

/// Run every topic through the engine; documents are named by their file stem (`steuer-01`).
pub fn evaluate(e: &Engine, topics: &[Topic]) -> Value {
    let g = match e.generation() {
        Some(g) => g,
        None => return json!({"error": "index empty"}),
    };
    let name_of = |doc: u32| -> String {
        let p = &g.cat.docs[doc as usize].paths[0].path;
        Path::new(p).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    };
    let mut rows = Vec::new();
    let mut sum = [Metrics::default(), Metrics::default()];
    let mut hard_n = [0usize; 2];
    let mut partial = 0;
    let mut ms = Vec::new();
    for t in topics {
        let mut row = json!({"id": t.id, "name": t.name});
        for (k, (key, q)) in [("query", &t.query), ("query_alt", &t.query_alt)].into_iter().enumerate() {
            let req = SearchReq {
                query: q.clone(),
                limit: 10,
                kinds: None,
                roots: None,
                since: None,
                caller: Caller::Desktop,
                deadline: Duration::from_secs(30),
                hidden: vec![],
                force_doc: None,
            };
            let out = e.search(&req);
            ms.push(out.took_ms);
            if out.partial.is_some() {
                partial += 1;
            }
            let top: Vec<String> = out.hits.iter().map(|h| name_of(h.doc)).collect();
            let m = metrics(&top, t);
            sum[k].recall += m.recall;
            sum[k].precision += m.precision;
            sum[k].ndcg += m.ndcg;
            sum[k].mrr += m.mrr;
            sum[k].leakage += m.leakage;
            if let Some(h) = m.hard_recall {
                *sum[k].hard_recall.get_or_insert(0.0) += h;
                hard_n[k] += 1;
            }
            row[key] = json!({"recall10": m.recall, "precision10": m.precision, "ndcg10": m.ndcg, "mrr": m.mrr,
                "leakage": m.leakage, "hard_recall10": m.hard_recall, "top10": top});
        }
        rows.push(row);
    }
    let n = topics.len().max(1) as f64;
    let mean = |k: usize| {
        json!({"recall10": sum[k].recall / n, "precision10": sum[k].precision / n, "ndcg10": sum[k].ndcg / n,
               "mrr": sum[k].mrr / n, "leakage": sum[k].leakage / n,
               "hard_recall10": sum[k].hard_recall.map(|h| h / hard_n[k].max(1) as f64)})
    };
    ms.sort();
    json!({
        "embedder": g.cat.embedder, "docs": g.cat.docs.len(), "chunks": g.cat.n_chunks, "calib": {"floor": g.cat.calib.floor, "ceil": g.cat.calib.ceil},
        "topics": rows, "mean": {"query": mean(0), "query_alt": mean(1)}, "partial_queries": partial,
        "search_ms": {"p50": ms.get(ms.len() / 2), "max": ms.last()},
    })
}

/// A plain table for the terminal and the notes.
pub fn table(report: &Value) -> String {
    let mut s = format!("embedder: {}\n", report["embedder"].as_str().unwrap_or("none (keyword only)"));
    s.push_str("topic       q:R@10  P@10  nDCG   MRR  leak hard | alt:R@10  P@10  nDCG   MRR  leak hard\n");
    let f = |v: &Value| v.as_f64().map(|x| format!("{x:5.2}")).unwrap_or_else(|| "    -".into());
    let row = |name: &str, q: &Value, a: &Value| {
        format!(
            "{name:10} {} {} {} {} {} {} | {} {} {} {} {} {}\n",
            f(&q["recall10"]),
            f(&q["precision10"]),
            f(&q["ndcg10"]),
            f(&q["mrr"]),
            f(&q["leakage"]),
            f(&q["hard_recall10"]),
            f(&a["recall10"]),
            f(&a["precision10"]),
            f(&a["ndcg10"]),
            f(&a["mrr"]),
            f(&a["leakage"]),
            f(&a["hard_recall10"])
        )
    };
    for t in report["topics"].as_array().into_iter().flatten() {
        s.push_str(&row(t["id"].as_str().unwrap_or(""), &t["query"], &t["query_alt"]));
    }
    s.push_str(&row("MEAN", &report["mean"]["query"], &report["mean"]["query_alt"]));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capped_recall_and_mrr() {
        let t = Topic {
            id: "x".into(),
            name: "x".into(),
            query: "q".into(),
            query_alt: "q".into(),
            relevant: (0..14).map(|i| format!("r{i}")).collect(),
            near_distractors: vec!["n".into()],
            hard: vec!["r0".into()],
        };
        let mut top: Vec<String> = (0..10).map(|i| format!("r{i}")).collect();
        let m = metrics(&top, &t);
        assert_eq!(m.recall, 1.0, "capped at 10");
        assert!((m.ndcg - 1.0).abs() < 1e-9);
        top[0] = "n".into();
        let m = metrics(&top, &t);
        assert_eq!(m.leakage, 1.0);
        assert_eq!(m.mrr, 0.5);
        assert_eq!(m.hard_recall, Some(0.0));
    }
}
