//! Brief CX-1: recall@10 per topic on host/cx-eval. The keyword part alone must beat the BM25 floor of
//! `baseline.py` (0.42 / 0.34); with the real model (when built in third_party/) the hybrid must reach the
//! CX-1 numbers in plan/notes/CX-1.md (a regression guard; CX-2 tunes and sets the targets).

mod common;
use common::*;
use ctxd::config::{Config, RootCfg};
use ctxd::engine::{Engine, EngineOpts};
use ctxd::eval;

fn run(kind: &str) -> serde_json::Value {
    let d = tmp(&format!("eval-{kind}"));
    let mut cfg = Config::default();
    cfg.context.roots = vec![RootCfg { path: eval_docs(), private: false }];
    cfg.context.embedder.kind = kind.into();
    let e = Engine::open(cfg.context, &d.join("data"), EngineOpts { worker_exe: Some(bin()) }).unwrap();
    let o = e.index_now().unwrap();
    assert_eq!(o.docs, 151);
    assert_eq!(o.failed, 0, "every test document extracts");
    let topics = eval::load_topics(&repo().join("host/cx-eval/topics.json")).unwrap();
    let rep = eval::evaluate(&e, &topics);
    eprintln!("{}", eval::table(&rep));
    rep
}

fn mean(rep: &serde_json::Value, k: &str) -> f64 {
    rep["mean"][k]["recall10"].as_f64().unwrap()
}

#[test]
fn keyword_only_beats_baseline() {
    let rep = run("none");
    assert!(mean(&rep, "query") > 0.42 && mean(&rep, "query_alt") > 0.34, "{}", eval::table(&rep));
    assert_eq!(rep["partial_queries"], 20, "without a model every answer says partial");
}

#[test]
fn fake_embedder_pipeline() {
    let rep = run("fake");
    assert!(mean(&rep, "query") > 0.42, "{}", eval::table(&rep));
    assert_eq!(rep["partial_queries"], 0);
}

#[test]
fn real_model_recall() {
    if skip_without_model("real_model_recall") {
        return;
    }
    let rep = run("llama");
    assert_eq!(rep["partial_queries"], 0);
    // Measured 15.10. on the Gaming-PC: 0.66 / 0.51 (plan/notes/CX-1.md); margin for float noise.
    assert!(mean(&rep, "query") >= 0.62 && mean(&rep, "query_alt") >= 0.47, "{}", eval::table(&rep));
    for t in rep["topics"].as_array().unwrap() {
        assert!(t["query"]["mrr"].as_f64().unwrap() >= 0.5, "first relevant hit in the top 2: {}", t["id"]);
    }
}
