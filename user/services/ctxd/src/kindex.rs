//! Keyword index (tantivy, BM25) over chunks: fields title and name (file name + path segments, boosted
//! x2.5) and body, all with the German 4-gram tokenizer. One tantivy document per chunk.

use crate::tokenize::{GermanTokenizer, TOKENIZER_NAME};
use std::path::Path;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, BoostQuery, Occur, Query, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, NumericOptions, Schema, TextFieldIndexing, TextOptions, Value};
use tantivy::{Index, IndexReader, ReloadPolicy, TantivyDocument, Term, doc};

pub const FIELD_BOOST: f32 = 2.5;

pub struct KIndex {
    reader: IndexReader,
    chunk: Field,
    fields: [(Field, f32); 3],
}

fn schema() -> (Schema, Field, [Field; 3]) {
    let mut b = Schema::builder();
    let chunk = b.add_u64_field("chunk", NumericOptions::default().set_stored());
    let text = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default().set_tokenizer(TOKENIZER_NAME).set_index_option(IndexRecordOption::WithFreqs),
    );
    let title = b.add_text_field("title", text.clone());
    let name = b.add_text_field("name", text.clone());
    let body = b.add_text_field("body", text);
    (b.build(), chunk, [title, name, body])
}

fn register(index: &Index) {
    index.tokenizers().register(TOKENIZER_NAME, GermanTokenizer::default());
}

pub struct Entry<'a> {
    pub chunk: u32,
    pub title: &'a str,
    pub name: &'a str,
    pub body: &'a str,
}

pub fn build<'a>(dir: &Path, entries: impl Iterator<Item = Entry<'a>>, threads: usize) -> tantivy::Result<()> {
    std::fs::create_dir_all(dir)?;
    let (schema, chunk, [title, name, body]) = schema();
    let index = Index::create_in_dir(dir, schema)?;
    register(&index);
    // One writer thread: the segment layout and so the scores do not depend on thread timing (a rebuild
    // gives the same index); the keyword index is a small part of the indexing time.
    let _ = threads;
    let mut w = index.writer_with_num_threads::<TantivyDocument>(1, 64_000_000)?;
    for e in entries {
        w.add_document(doc!(chunk => e.chunk as u64, title => e.title, name => e.name, body => e.body))?;
    }
    w.commit()?;
    w.wait_merging_threads()?;
    Ok(())
}

impl KIndex {
    pub fn open(dir: &Path) -> tantivy::Result<KIndex> {
        let index = Index::open_in_dir(dir)?;
        register(&index);
        let (_, _, _) = schema();
        let s = index.schema();
        let chunk = s.get_field("chunk")?;
        let fields =
            [(s.get_field("title")?, FIELD_BOOST), (s.get_field("name")?, FIELD_BOOST), (s.get_field("body")?, 1.0)];
        let reader = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into()?;
        Ok(KIndex { reader, chunk, fields })
    }

    /// The best `max_hits` chunks for the query, with their BM25 scores.
    pub fn search(&self, query: &str, max_hits: usize) -> tantivy::Result<Vec<(u32, f32)>> {
        let mut terms = crate::tokenize::term_list(query);
        terms.sort();
        terms.dedup();
        if terms.is_empty() || max_hits == 0 {
            return Ok(Vec::new());
        }
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for t in &terms {
            for (f, boost) in self.fields {
                let q: Box<dyn Query> =
                    Box::new(TermQuery::new(Term::from_field_text(f, t), IndexRecordOption::WithFreqs));
                let q: Box<dyn Query> = if boost != 1.0 { Box::new(BoostQuery::new(q, boost)) } else { q };
                clauses.push((Occur::Should, q));
            }
        }
        let q = BooleanQuery::new(clauses);
        let searcher = self.reader.searcher();
        let top = searcher.search(&q, &TopDocs::with_limit(max_hits).order_by_score())?;
        let mut out = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let d: TantivyDocument = searcher.doc(addr)?;
            if let Some(c) = d.get_first(self.chunk).and_then(|v| v.as_u64()) {
                out.push((c as u32, score));
            }
        }
        Ok(out)
    }

    pub fn num_docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_boost_and_compounds() {
        let d = std::env::temp_dir().join(format!("ctxd-kindex-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let docs = [
            (0, "Stromliefervertrag", "strom.txt", "Vertrag mit den Stadtwerken über Strom."),
            (1, "Einkaufsliste", "liste.txt", "Milch, Brot, Strom-Adapter."),
            (2, "Urlaub", "rom.md", "Rom im März, Kolosseum."),
        ];
        build(&d, docs.iter().map(|(c, t, n, b)| Entry { chunk: *c, title: t, name: n, body: b }), 1).unwrap();
        let k = KIndex::open(&d).unwrap();
        assert_eq!(k.num_docs(), 3);
        let r = k.search("Stromvertrag", 10).unwrap();
        assert_eq!(r[0].0, 0, "{r:?}");
        assert!(r.iter().all(|(c, _)| *c != 2));
        assert!(k.search("und der die", 10).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}
