//! Extraction of PDF/DOCX/ODT (fixtures made from the test set with LibreOffice) and hostile input
//! (DESIGN §11.2): truncated PDF, zip bomb, invalid UTF-8, a symlink loop, a symlink out of the root, a name
//! with a newline, a 2 GB sparse file, a mail with 10 000 headers. Nothing panics, the bad file gets
//! `failed:<reason>`, the rest is indexed. Extraction runs in worker processes as in the service.

mod common;
use common::*;
use ctxd::config::{Config, RootCfg};
use ctxd::engine::{Caller, Engine, EngineOpts};
use ctxd::extract::Limits;
use ctxd::extract::pool::{Isolation, Pool};
use serde_json::json;
use std::io::Write;
use std::time::Duration;

fn pool() -> Pool {
    Pool::new(Isolation::Process { exe: bin(), mem_mb: 1024 }, 2, Duration::from_secs(20), Limits::default())
}

#[test]
fn office_and_pdf_fixtures() {
    let p = pool();
    let cases = [
        ("lohnsteuer.pdf", "pdf", "Einbehaltene Lohnsteuer"),
        ("mietvertrag.pdf", "pdf", "Betriebskostenabrechnung"),
        ("lohnsteuer.docx", "docx", "Bruttoarbeitslohn"),
        ("gartenplan.docx", "docx", "Hochbeet"),
        ("gartenplan.odt", "odt", "Radieschen"),
        ("mietvertrag.odt", "odt", "Grundsteuer"),
    ];
    for (f, kind, word) in cases {
        let e = p.extract(&fixtures().join(f)).unwrap_or_else(|e| panic!("{f}: {e}"));
        assert_eq!(e.kind, kind, "{f}");
        assert!(e.text.contains(word), "{f}: {}", e.text);
        assert!(e.title.is_some(), "{f}");
    }
}

#[test]
fn hostile_files_fail_alone() {
    let d = tmp("hostile");
    let root = d.join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("gut.txt"), "Ein ganz normaler Brief über die Steuererklärung.").unwrap();
    let pdf = std::fs::read(fixtures().join("lohnsteuer.pdf")).unwrap();
    std::fs::write(root.join("kaputt.pdf"), &pdf[..pdf.len() / 3]).unwrap();
    std::fs::write(root.join("muell.pdf"), b"%PDF-1.7\n\xff\xfe garbage \x00\x01 trailer <<>>").unwrap();
    // Zip bomb: 300 MB of one letter, a few hundred KB packed (under the 1 MB file limit).
    {
        let f = std::fs::File::create(root.join("bombe.docx")).unwrap();
        let mut z = zip::ZipWriter::new(f);
        z.start_file("word/document.xml", zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(b"<w:document><w:p><w:t>").unwrap();
        let block = vec![b'a'; 1 << 20];
        for _ in 0..300 {
            z.write_all(&block).unwrap();
        }
        z.finish().unwrap();
    }
    std::fs::write(root.join("kaputt.odt"), b"PK\x03\x04 not really a zip").unwrap();
    std::fs::write(root.join("latin1.txt"), b"Gr\xfc\xdfe vom Finanzamt \xff\xfe\x00").unwrap();
    let mut mail = String::new();
    for i in 0..10_000 {
        mail.push_str(&format!("X-Kopf-{i}: wert\r\n"));
    }
    mail.push_str("Subject: Viele Kopfzeilen\r\n\r\nInhalt der Mail.\r\n");
    std::fs::write(root.join("koepfe.eml"), mail).unwrap();
    std::fs::write(root.join("zeile\numbruch.txt"), "Name mit Zeilenumbruch").unwrap();
    std::os::unix::fs::symlink(&root, root.join("schleife")).unwrap();
    let outside = d.join("draussen");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("geheim.txt"), "Darf nie gelesen werden: Kassenschrank").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("ausgang")).unwrap();
    std::os::unix::fs::symlink(outside.join("geheim.txt"), root.join("geheim-link.txt")).unwrap();
    let sparse = std::fs::File::create(root.join("riesig.txt")).unwrap();
    sparse.set_len(2 << 30).unwrap();
    drop(sparse);
    std::fs::create_dir_all(root.join(".ssh")).unwrap();
    std::fs::write(root.join(".ssh/id_ed25519"), "PRIVATE KEY").unwrap();
    std::fs::write(root.join("tresor.kdbx"), "x").unwrap();

    let mut cfg = Config::default();
    cfg.context.roots = vec![RootCfg { path: root.clone(), private: false }];
    cfg.context.embedder.kind = "fake".into();
    // 1 MB of text per file at most: the bomb is cut there (the default is 64 MB).
    cfg.context.index.max_file_mb = 1;
    let e = Engine::open(cfg.context, &d.join("data"), EngineOpts { worker_exe: Some(bin()) }).unwrap();
    let out = e.index_now().unwrap();
    let g = e.generation().unwrap();
    let names: Vec<String> = g
        .cat
        .docs
        .iter()
        .flat_map(|d| d.paths.iter().map(|p| p.path.rsplit('/').next().unwrap().to_string()))
        .collect();
    for n in [
        "gut.txt",
        "kaputt.pdf",
        "muell.pdf",
        "bombe.docx",
        "kaputt.odt",
        "latin1.txt",
        "koepfe.eml",
        "zeile\numbruch.txt",
    ] {
        assert!(names.contains(&n.to_string()), "{n} missing: {names:?}");
    }
    for n in ["geheim.txt", "geheim-link.txt", "riesig.txt", "id_ed25519", "tresor.kdbx"] {
        assert!(!names.contains(&n.to_string()), "{n} must not be indexed");
    }
    let status = |n: &str| g.cat.docs.iter().find(|d| d.paths[0].path.ends_with(n)).unwrap().status.clone();
    assert!(status("kaputt.pdf").starts_with("failed:") || status("kaputt.pdf") == "empty", "{}", status("kaputt.pdf"));
    assert!(status("kaputt.odt").starts_with("failed:"));
    assert_eq!(status("gut.txt"), "ok");
    assert_eq!(status("koepfe.eml"), "ok");
    assert_eq!(status("latin1.txt"), "ok");
    let bomb = g.cat.docs.iter().find(|d| d.paths[0].path.ends_with("bombe.docx")).unwrap();
    assert!(bomb.warnings.contains(&"text_truncated".to_string()) || bomb.status.starts_with("failed:"), "{bomb:?}");
    assert!(out.failed >= 2);
    // The good document is findable, the umlauts of the Windows-1252 file too.
    let r = ctxd::api::call(&e, "context_search", &json!({"query": "Grüße Finanzamt"}), Caller::Desktop).unwrap();
    assert!(r["items"].as_array().unwrap().iter().any(|i| i["path"].as_str().unwrap().ends_with("latin1.txt")), "{r}");
    let st = ctxd::api::call(&e, "context_status", &json!({}), Caller::Desktop).unwrap();
    assert!(st["failed"].as_array().unwrap().len() >= 2, "{st}");
    assert_eq!(st["scan"]["too_large"], json!(1));
    assert!(st["scan"]["symlinks"].as_u64().unwrap() >= 3);
}

#[test]
fn forbidden_roots_are_refused() {
    let d = tmp("forbidden");
    let ok = d.join("ok");
    let bad = d.join("windows-kopie");
    std::fs::create_dir_all(&ok).unwrap();
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(ok.join("a.txt"), "erlaubt").unwrap();
    std::fs::write(bad.join("b.txt"), "verboten").unwrap();
    let link = d.join("umweg");
    std::os::unix::fs::symlink(&bad, &link).unwrap();
    let mut cfg = Config::default();
    cfg.context.roots = vec![
        RootCfg { path: ok.clone(), private: false },
        RootCfg { path: link, private: false },
        RootCfg { path: "/run/media".into(), private: false },
    ];
    cfg.context.forbidden_roots = vec![bad];
    cfg.context.embedder.kind = "none".into();
    let e = Engine::open(cfg.context, &d.join("data"), EngineOpts { worker_exe: None }).unwrap();
    e.index_now().unwrap();
    let g = e.generation().unwrap();
    assert_eq!(g.cat.docs.len(), 1);
    assert_eq!(g.cat.denied.len(), 2, "{:?}", g.cat.denied);
}
